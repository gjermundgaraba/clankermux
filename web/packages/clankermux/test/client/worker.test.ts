/**
 * The worker as the page drives it, against a server played by the test:
 * the page's requests become the right protocol requests on the one
 * connection, their outcomes come back by id, each mounted terminal has
 * its own attachment on that connection, and its link reports what
 * happened to its session.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";

import { Engine, type KeyInput } from "../../src/engine/index.ts";
import { type Frame, FrameType, type Session, encodeFrame, parseFrame } from "../../src/proto/index.ts";

import type { ToMount } from "../../src/client/mount.ts";
import { type Link, type ToPage, type ToWorker, runWorker } from "../../src/client/worker.ts";
import { output, session, welcome } from "./bodies.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const wasm = readFileSync(`${here}../../ghostty-vt.wasm`);
let engine: Engine;
beforeAll(async () => {
  engine = await Engine.load(wasm);
});

/** The browser's WebSocket as the worker uses it; the test is the server. */
class FakeSocket {
  static all: FakeSocket[] = [];
  binaryType = "";
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: ArrayBuffer }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  sent: Frame[] = [];

  constructor(readonly url: string) {
    FakeSocket.all.push(this);
    queueMicrotask(() => this.onopen?.());
  }

  send(bytes: Uint8Array): void {
    this.sent.push(parseFrame(bytes, 1 << 20));
  }

  close(): void {
    this.onclose?.();
  }

  push(type: FrameType, channel: number, body: unknown): void {
    const bytes = encodeFrame(type, channel, new TextEncoder().encode(JSON.stringify(body)));
    this.pushRaw(bytes);
  }

  pushRaw(bytes: Uint8Array): void {
    this.onmessage?.({ data: bytes.slice().buffer });
  }

  /** The JSON bodies sent with `type`. */
  bodies(type: FrameType): { channel: number; body: Record<string, unknown> }[] {
    return this.sent
      .filter((f) => f.header.type === type)
      .map((f) => ({ channel: f.header.channel, body: JSON.parse(new TextDecoder().decode(f.payload)) }));
  }

  /** The first request with `method` and an id above `after`, once sent. */
  async request(method: string, after = 0): Promise<{ id: number; params: Record<string, unknown> }> {
    let found: { id: number; params: Record<string, unknown> } | undefined;
    await vi.waitFor(() => {
      found = this.bodies(FrameType.Request)
        .map((r) => r.body as { id: number; method: string; params: Record<string, unknown> })
        .find((r) => r.method === method && r.id > after);
      expect(found).toBeDefined();
    });
    return found!;
  }

  /** Answers `HELLO`. */
  async welcome(incarnation = "i"): Promise<void> {
    await vi.waitFor(() => expect(this.bodies(FrameType.Hello)).toHaveLength(1));
    this.push(FrameType.Welcome, 0, welcome(incarnation));
  }

  /** Answers `HELLO` and the `subscribe`: with `sessions` as its first
   * page, or, for a `subscribe` after a reconnect, with `gap`. */
  async serve(sessions: Session[] = [], { incarnation = "i", latest = 0, gap = false } = {}): Promise<void> {
    await this.welcome(incarnation);
    const subscribe = await this.request("subscribe");
    const result =
      subscribe.params.after === undefined ? { sessions, oldest: 1, latest } : { oldest: 1, latest, gap };
    this.push(FrameType.Reply, 0, { id: subscribe.id, result });
    if (subscribe.params.after !== undefined && gap) {
      const list = await this.request("list", subscribe.id);
      this.push(FrameType.Reply, 0, { id: list.id, result: { sessions } });
    }
  }

  /** The channel of the latest `OPEN` for `session`. */
  async opened(session: string): Promise<number> {
    let channel = 0;
    await vi.waitFor(() => {
      const open = this.bodies(FrameType.Open)
        .reverse()
        .find((o) => o.body.session === session);
      expect(open).toBeDefined();
      channel = open!.channel;
    });
    return channel;
  }
}

/** A whole snapshot bootstrap of `s` on `channel`, its terminal showing `text`. */
function bootstrap(socket: FakeSocket, channel: number, s: Session, text = "hello"): void {
  const source = engine.terminalNew(10, 3);
  source.write(new TextEncoder().encode(text));
  const bytes = source.snapshot();
  source.free();
  socket.push(FrameType.Attached, channel, {
    mode: "snapshot",
    offset: 0,
    marker: 0,
    cols: 10,
    rows: 3,
    session: s,
    snapshot_bytes: bytes.length,
  });
  socket.pushRaw(encodeFrame(FrameType.Snapshot, channel, bytes));
  socket.pushRaw(encodeFrame(FrameType.SnapshotReady, channel, new Uint8Array()));
  socket.pushRaw(encodeFrame(FrameType.SnapshotEnd, channel, new Uint8Array()));
}

type Message<T extends ToPage["type"]> = Extract<ToPage, { type: T }>;

/** A worker on a port as the page's: what it posted, and a send to it. */
function run() {
  const posted: ToPage[] = [];
  const port = {
    postMessage: (message: ToPage) => posted.push(message),
    onmessage: null as ((e: MessageEvent<ToWorker>) => void) | null,
  };
  runWorker(port);
  const send = (message: ToWorker) => port.onmessage!({ data: message } as MessageEvent<ToWorker>);
  return { posted, send };
}

/** A worker started as `connect` starts it, once its engine is loaded,
 * and what it posted. */
async function start() {
  const { posted, send } = run();
  send({ type: "init", wasm: "ghostty-vt.wasm", url: "wss://127.0.0.1:7438/", client: undefined });
  await vi.waitFor(() => expect(posted).toContainEqual({ type: "ready" }), { timeout: 5000 });
  let nextId = 100;
  const of = <T extends ToPage["type"]>(type: T) => posted.filter((m): m is Message<T> => m.type === type);
  return {
    send,
    posted,
    /** Mounts terminal `mount`, as `client.mount` does. */
    mount(mount: number, session?: string, extra: { active?: boolean; readOnly?: boolean; ackOnFocus?: boolean } = {}) {
      send({
        type: "mount",
        mount,
        session,
        viewport: { cols: 80, rows: 24 },
        profile: {},
        active: extra.active ?? true,
        readOnly: extra.readOnly ?? false,
        ackOnFocus: extra.ackOnFocus ?? false,
      });
    },
    /** A message for terminal `mount`. */
    to(mount: number, message: ToMount) {
      send({ ...message, mount } as ToWorker);
    },
    /** A request as `Client` sends it; its reply, once posted. */
    request(method: string, params: unknown) {
      const id = nextId++;
      send({ type: "request", id, method, params });
      return {
        id,
        reply: async () => {
          let found: Message<"reply"> | undefined;
          await vi.waitFor(() => {
            found = of("reply").find((m) => m.id === id);
            expect(found).toBeDefined();
          });
          return found!;
        },
      };
    },
    /** The latest frame's scrollbar of terminal `mount`, once one was posted. */
    scrollbar: (mount = 1) => of("frame").filter((m) => m.mount === mount).at(-1)?.frame.scrollbar,
    links: (mount = 1) => of("link").flatMap((m) => (m.mount === mount ? [m.link] : [])),
    screens: (mount = 1) => of("status").flatMap((m) => (m.mount === mount ? [m.text] : [])),
    selections: (mount = 1) => of("selection").filter((m) => m.mount === mount),
    events: () => of("event").map((m) => m.event),
    sessions: () => of("sessions").at(-1)?.sessions,
  };
}

/** A key press, or its release. */
function key(code: string, extra: Partial<KeyInput> = {}): KeyInput {
  return { code, key: code, action: "press", shift: false, ctrl: false, alt: false, meta: false, ...extra };
}

/** The `index`th socket, once the worker opened it: the first waits for
 * the engine's compile, so it gets a few seconds. */
async function socket(index: number): Promise<FakeSocket> {
  await vi.waitFor(() => expect(FakeSocket.all.length).toBeGreaterThan(index), { timeout: 5000 });
  return FakeSocket.all[index]!;
}

beforeEach(() => {
  FakeSocket.all = [];
  vi.stubGlobal("WebSocket", FakeSocket);
  vi.stubGlobal("fetch", async () => ({ arrayBuffer: async () => wasm }));
});

afterEach(() => {
  vi.unstubAllGlobals();
});

test("a request goes as the protocol request of the same name, and its result or error comes back by id", async () => {
  const worker = await start();
  const server = await socket(0);
  await server.serve();

  const create = worker.request("create", { argv: ["/bin/cat"], on_exit: "keep" });
  const asked = await server.request("create");
  expect(asked.params).toEqual({ argv: ["/bin/cat"], on_exit: "keep" });
  server.push(FrameType.Reply, 0, { id: asked.id, result: session("a") });
  expect(await create.reply()).toEqual({ type: "reply", id: create.id, result: session("a") });

  const end = worker.request("end", { session: "a" });
  const ended = await server.request("end", asked.id);
  server.push(FrameType.Error, 0, { id: ended.id, code: "not_running", message: "session is not running" });
  expect((await end.reply()).error).toEqual({
    kind: "server",
    message: "not_running: session is not running",
    server: { id: ended.id, code: "not_running", message: "session is not running" },
  });
});

test("a request with no connection fails without asking the server", async () => {
  const worker = await start();
  const request = worker.request("inspect", { session: "s" });
  expect((await request.reply()).error).toEqual({ kind: "closed", message: "not connected" });
  const server = await socket(0);
  await server.serve();
  expect(server.bodies(FrameType.Request).map((r) => r.body.method)).toEqual(["subscribe"]);
});

test("a wait cancelled by the page is cancelled on the server by its own request id, and rejects with cancelled", async () => {
  const worker = await start();
  const server = await socket(0);
  await server.serve();
  const wait = worker.request("wait", { session: "s", until: { exited: true } });
  const waiting = await server.request("wait");
  expect(waiting.params).toEqual({ session: "s", until: { exited: true } });

  worker.send({ type: "cancel", id: wait.id });
  const cancel = await server.request("cancel");
  expect(cancel.params).toEqual({ id: waiting.id });
  server.push(FrameType.Error, 0, { id: waiting.id, code: "cancelled", message: "cancelled" });
  server.push(FrameType.Reply, 0, { id: cancel.id, result: { cancelled: true } });
  expect((await wait.reply()).error?.server?.code).toBe("cancelled");

  // A request no longer pending is not cancelled again.
  worker.send({ type: "cancel", id: wait.id });
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(server.bodies(FrameType.Request).filter((r) => r.body.method === "cancel")).toHaveLength(1);
});

test("two terminals share the one connection, each with its own channel, screen and input", async () => {
  const worker = await start();
  worker.mount(1, "s");
  worker.mount(2, "t");
  const server = await socket(0);
  await server.serve([session("s"), session("t")]);
  const s = await server.opened("s");
  const t = await server.opened("t");
  expect(s).not.toBe(t);
  bootstrap(server, s, session("s"), "left");
  bootstrap(server, t, session("t"), "right");
  await vi.waitFor(() => {
    expect(worker.links(1).at(-1)).toEqual({ state: "connected", session: session("s") });
    expect(worker.links(2).at(-1)).toEqual({ state: "connected", session: session("t") });
  });

  server.pushRaw(encodeFrame(FrameType.Output, t, output(0, "\r\nmore")));
  await vi.waitFor(() => expect(worker.scrollbar(2)).toEqual({ total: 3, offset: 0, len: 3 }));
  worker.to(1, { type: "key", input: key("KeyA", { key: "a" }) });
  worker.to(2, { type: "key", input: key("KeyB", { key: "b" }) });
  await vi.waitFor(() =>
    expect(server.sent.filter((f) => f.header.type === FrameType.Input).map((f) => [f.header.channel, new TextDecoder().decode(f.payload)])).toEqual([
      [s, "a"],
      [t, "b"],
    ]),
  );
  expect(FakeSocket.all).toHaveLength(1);

  // One terminal's end leaves the other attached.
  worker.send({ type: "unmount", mount: 1 });
  await vi.waitFor(() => expect(server.sent.some((f) => f.header.type === FrameType.Close && f.header.channel === s)).toBe(true));
  server.push(FrameType.Exited, t, { seq: 1, offset: 6, exit: { code: 0 } });
  await vi.waitFor(() => expect(worker.links(2).at(-1)).toEqual({ state: "ended", reason: "exited", exit: { code: 0 }, retry: false }));
  expect(worker.links(1).at(-1)).toEqual({ state: "connected", session: session("s") });
});

test("a terminal mounted later sees the connection as it is, up or retrying, and connects again one that ended for good", async () => {
  const worker = await start();
  worker.mount(1);
  const first = await socket(0);
  await vi.waitFor(() => expect(first.bodies(FrameType.Hello)).toHaveLength(1));
  first.close();
  await vi.waitFor(() => expect(worker.links(1).at(-1)).toEqual({ state: "connecting", attempt: 2 }));
  worker.mount(2);
  expect(worker.links(2)).toEqual([{ state: "connecting", attempt: 2 }]);

  await (await socket(1)).serve([session("s")]);
  await vi.waitFor(() => expect(worker.links(1).at(-1)).toEqual({ state: "connected", session: undefined }));

  // An established connection lost: a terminal mounted before the next
  // attempt sees it connecting, not connected.
  FakeSocket.all[1]!.close();
  await vi.waitFor(() => expect(worker.links(1).at(-1)).toEqual({ state: "ended", reason: "closed", retry: true }));
  worker.mount(5);
  expect(worker.links(5)).toEqual([{ state: "connecting", attempt: 1 }]);
  await (await socket(2)).serve([session("s")]);
  await vi.waitFor(() => expect(worker.links(5).at(-1)).toEqual({ state: "connected", session: undefined }));
  worker.mount(3, "s");
  const channel = await FakeSocket.all[2]!.opened("s");
  bootstrap(FakeSocket.all[2]!, channel, session("s"));
  await vi.waitFor(() => expect(worker.links(3)).toEqual([{ state: "connected", session: session("s") }]));

  FakeSocket.all[2]!.push(FrameType.Error, 0, { code: "protocol_error", message: "bad frame" });
  const refused = { state: "ended", reason: "error", code: "protocol_error", message: "bad frame", retry: false };
  await vi.waitFor(() => expect(worker.links(1).at(-1)).toEqual(refused));
  expect(worker.links(3).at(-1)).toEqual(refused);
  // A terminal mounted now connects again, for every terminal.
  worker.mount(4);
  expect(worker.links(4)).toEqual([{ state: "connecting", attempt: 1 }]);
  expect(worker.links(1).at(-1)).toEqual({ state: "connecting", attempt: 1 });
  await (await socket(3)).serve();
  await vi.waitFor(() => expect(worker.links(4).at(-1)).toEqual({ state: "connected", session: undefined }));
});

test("an attach while the connection is still subscribing opens the session once, when it is up", async () => {
  const worker = await start();
  worker.mount(1);
  const server = await socket(0);
  await server.welcome();
  const subscribe = await server.request("subscribe");
  worker.to(1, { type: "attach", session: "s" });
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(server.bodies(FrameType.Open)).toEqual([]);
  server.push(FrameType.Reply, 0, { id: subscribe.id, result: { sessions: [session("s")], oldest: 1, latest: 0 } });
  await server.opened("s");
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(server.bodies(FrameType.Open)).toHaveLength(1);
});

test("the connection's own link reaches the page: connecting, connected, a loss, connecting again", async () => {
  const worker = await start();
  const server = await socket(0);
  await server.serve();
  const connection = () => worker.posted.flatMap((m) => (m.type === "connection" ? [m.link] : []));
  await vi.waitFor(() =>
    expect(connection()).toEqual<Link[]>([
      { state: "connecting", attempt: 1 },
      { state: "connected", session: undefined },
    ]),
  );
  server.close();
  await vi.waitFor(() =>
    expect(connection().slice(2)).toEqual<Link[]>([
      { state: "ended", reason: "closed", retry: true },
      { state: "connecting", attempt: 1 },
    ]),
  );
});

test("after a reconnect each terminal resumes its stream and the events missed meanwhile are replayed", async () => {
  const worker = await start();
  worker.mount(1, "s");
  worker.mount(2, "t");
  const server = await socket(0);
  await server.serve([session("s"), session("t")], { latest: 5 });
  bootstrap(server, await server.opened("s"), session("s"), "one");
  bootstrap(server, await server.opened("t"), session("t"), "two");
  await vi.waitFor(() => expect(worker.links(2).at(-1)?.state).toBe("connected"));
  const bell = { seq: 6, event: "bell", id: "s" };
  server.push(FrameType.Event, 0, bell);
  await vi.waitFor(() => expect(worker.events()).toEqual([bell]));

  server.close();
  const next = await socket(1);
  const replayed = { seq: 7, event: "bell", id: "t" };
  const done = next.serve([session("s"), session("t")], { latest: 7 });
  const subscribe = await next.request("subscribe");
  expect(subscribe.params).toEqual({ after: 6, incarnation: "i" });
  await done;
  next.push(FrameType.Event, 0, replayed);
  await vi.waitFor(() => expect(worker.events()).toEqual([bell, replayed]));
  expect(next.bodies(FrameType.Request).map((r) => r.body.method)).toEqual(["subscribe"]);

  for (const [mount, id] of [[1, "s"], [2, "t"]] as const) {
    const channel = await next.opened(id);
    expect(next.bodies(FrameType.Open).find((o) => o.channel === channel)?.body.resume).toEqual({
      incarnation: "i",
      offset: 0,
      marker: 0,
    });
    next.push(FrameType.Attached, channel, { mode: "resume", offset: 0, marker: 0, cols: 10, rows: 3, session: session(id) });
    await vi.waitFor(() => expect(worker.links(mount).slice(-3)).toEqual<Link[]>([
      { state: "ended", reason: "closed", retry: true },
      { state: "connecting", attempt: 1 },
      { state: "connected", session: session(id) },
    ]));
    expect(worker.screens(mount).at(-1)).toBe("resumed");
  }
});

test("after a restart the events cannot be replayed: the gap is posted and the sessions paged again", async () => {
  const worker = await start();
  const server = await socket(0);
  await server.serve([session("s"), session("t")], { latest: 3 });
  await vi.waitFor(() => expect(worker.sessions()).toEqual([session("s"), session("t")]));
  server.close();
  const next = await socket(1);
  await next.serve([session("t", { status: "restored" })], { incarnation: "j", latest: 2, gap: true });
  await vi.waitFor(() => expect(worker.posted).toContainEqual({ type: "gap", incarnation: "j" }));
  expect(worker.sessions()).toEqual([session("t", { status: "restored" })]);
  expect(next.bodies(FrameType.Request).at(0)?.body.params).toEqual({ after: 3, incarnation: "i" });

  // The next reconnect replays from the new incarnation.
  next.close();
  const third = await socket(2);
  void third.serve([], { incarnation: "j", latest: 2 });
  expect((await third.request("subscribe")).params).toEqual({ after: 2, incarnation: "j" });
});

test("ack on focus: only when the page opted in, the terminal has the focus and its session wants the user with done or error", async () => {
  const worker = await start();
  worker.mount(1, "s", { ackOnFocus: true });
  worker.mount(2, "t");
  const server = await socket(0);
  const done = { ...session("s"), attention: { state: "done", record: "" } };
  const t = { ...session("t"), attention: { state: "done", record: "" } };
  await server.serve([done, t], { latest: 1 });
  bootstrap(server, await server.opened("s"), done);
  bootstrap(server, await server.opened("t"), t);
  await vi.waitFor(() => expect(worker.links(2).at(-1)?.state).toBe("connected"));
  const acks = () => server.bodies(FrameType.Request).filter((r) => r.body.method === "ack").map((r) => r.body.params);

  worker.to(2, { type: "focus", focused: true });
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(acks()).toEqual([]);
  worker.to(1, { type: "focus", focused: true });
  await vi.waitFor(() => expect(acks()).toEqual([{ session: "s" }]));
  // One at a time: a change while it is under way asks for no other.
  server.push(FrameType.Event, 0, { seq: 2, event: "session_changed", session: done });
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(acks()).toHaveLength(1);
  const first = await server.request("ack");
  server.push(FrameType.Reply, 0, { id: first.id, result: { record_ids: [""] } });

  // A record that wants the user while the terminal has the focus is
  // seen at once; a blocked one is not a record ack marks.
  const blocked = { ...session("s"), attention: { state: "blocked", record: "q" } };
  server.push(FrameType.Event, 0, { seq: 3, event: "session_changed", session: blocked });
  const error = { ...session("s"), attention: { state: "error", record: "e" } };
  server.push(FrameType.Event, 0, { seq: 4, event: "session_changed", session: error });
  await vi.waitFor(() => expect(acks()).toEqual([{ session: "s" }, { session: "s" }]));

  // Blurred: no more.
  worker.to(1, { type: "focus", focused: false });
  const second = await server.request("ack", first.id);
  server.push(FrameType.Reply, 0, { id: second.id, result: { record_ids: ["e"] } });
  await new Promise((resolve) => setTimeout(resolve, 20));
  server.push(FrameType.Event, 0, { seq: 5, event: "session_changed", session: done });
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(acks()).toHaveLength(2);
});

test("a read-only terminal opens read-only and sends no input", async () => {
  const worker = await start();
  worker.mount(1, "s", { readOnly: true });
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  expect(server.bodies(FrameType.Open).at(-1)!.body).toMatchObject({ session: "s", read_only: true });
  bootstrap(server, channel, session("s"));
  await vi.waitFor(() => expect(worker.scrollbar()).toBeDefined());
  worker.to(1, { type: "key", input: key("KeyA", { key: "a" }) });
  worker.to(1, { type: "paste", text: "pasted" });
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(server.sent.filter((f) => f.header.type === FrameType.Input)).toEqual([]);
  expect(server.bodies(FrameType.Request).map((r) => r.body.method)).toEqual(["subscribe"]);
});

test("detach leaves the terminal attached to none until it attaches again", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"));
  await vi.waitFor(() => expect(worker.links().at(-1)).toEqual({ state: "connected", session: session("s") }));
  worker.to(1, { type: "detach" });
  expect(server.sent.some((f) => f.header.type === FrameType.Close && f.header.channel === channel)).toBe(true);
  expect(worker.links().at(-1)).toEqual({ state: "connected", session: undefined });
  worker.to(1, { type: "attach", session: "s" });
  expect(await server.opened("s")).not.toBe(channel);
});

test("the link: connected to a session, its exit, then removed for good", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"));
  await vi.waitFor(() => expect(worker.links()).toContainEqual({ state: "connected", session: session("s") }));
  server.push(FrameType.Exited, channel, { seq: 1, offset: 0, exit: { code: 3 } });
  server.push(FrameType.Closed, channel, { reason: "removed" });
  await vi.waitFor(() =>
    expect(worker.links()).toEqual<Link[]>([
      { state: "connecting", attempt: 1 },
      { state: "connected", session: session("s") },
      { state: "ended", reason: "exited", exit: { code: 3 }, retry: false },
      { state: "ended", reason: "removed", retry: false },
    ]),
  );
});

test("the link: detached by another client, for good", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"));
  await vi.waitFor(() => expect(worker.links()).toContainEqual({ state: "connected", session: session("s") }));
  server.push(FrameType.Closed, channel, { reason: "detached", message: "taken over" });
  await vi.waitFor(() =>
    expect(worker.links().at(-1)).toEqual({ state: "ended", reason: "detached", message: "taken over", retry: false }),
  );
  // A reconnect does not attach the session again.
  server.close();
  const next = await socket(1);
  await next.serve([session("s")]);
  await vi.waitFor(() => expect(worker.links().at(-1)).toEqual({ state: "connected", session: undefined }));
  expect(next.bodies(FrameType.Open)).toEqual([]);
});

test("only a page in use reports its size: OPEN and VIEWPORT carry none until it is active", async () => {
  const worker = await start();
  worker.mount(1, "s", { active: false });
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  expect(server.bodies(FrameType.Open).at(-1)!.body).toEqual({ session: "s" });
  bootstrap(server, channel, session("s"));
  await vi.waitFor(() => expect(worker.links()).toContainEqual({ state: "connected", session: session("s") }));

  const size = { cols: 100, rows: 30 };
  worker.to(1, { type: "viewport", size, profile: { scheme: "dark" }, active: false });
  expect(server.bodies(FrameType.Viewport)).toEqual([]);
  // Focused again with the same size: reported, to take the size back.
  worker.to(1, { type: "viewport", size, profile: { scheme: "dark" }, active: true });
  expect(server.bodies(FrameType.Viewport)).toEqual([{ channel, body: { ...size, profile: { scheme: "dark" } } }]);
});

test("a program's title and working directory reach the page as they change", async () => {
  const worker = await start();
  const server = await socket(0);
  await server.serve([session("s")]);
  const changed = { ...session("s"), title: "vim notes.md", pwd: "/tmp/work" };
  const event = { seq: 1, event: "session_changed", session: changed };
  server.push(FrameType.Event, 0, event);
  await vi.waitFor(() => expect(worker.posted.slice(-2)).toEqual([{ type: "sessions", sessions: [changed] }, { type: "event", event }]));
});

test("every event reaches the page, whichever session it is of", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s"), session("t")]);
  const bells = [
    { seq: 2, event: "bell", id: "t" },
    { seq: 3, event: "bell", id: "s" },
  ];
  for (const bell of bells) server.push(FrameType.Event, 0, bell);
  await vi.waitFor(() => expect(worker.events()).toEqual(bells));
});

test("the link: an exited or refused session ends it, a server stopping is retried", async () => {
  const worker = await start();
  worker.mount(1, "old");
  const server = await socket(0);
  await server.serve([session("old", { status: "exited" })]);
  const exited = { ...session("old", { status: "exited" }), exit: { signal: "HUP" } };
  bootstrap(server, await server.opened("old"), exited);
  await vi.waitFor(() =>
    expect(worker.links().slice(1)).toEqual<Link[]>([
      { state: "connected", session: exited },
      { state: "ended", reason: "exited", exit: { signal: "HUP" }, retry: false },
    ]),
  );

  worker.to(1, { type: "attach", session: "gone" });
  server.push(FrameType.Error, await server.opened("gone"), { code: "no_such_session", message: "no session gone" });
  await vi.waitFor(() =>
    expect(worker.links().at(-1)).toEqual({
      state: "ended",
      reason: "no_such_session",
      code: "no_such_session",
      message: "no session gone",
      retry: false,
    }),
  );

  worker.to(1, { type: "attach", session: "s" });
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"));
  server.push(FrameType.Closed, channel, { reason: "stopping" });
  server.close();
  await vi.waitFor(() =>
    expect(worker.links().slice(-3)).toEqual<Link[]>([
      { state: "ended", reason: "stopping", retry: true },
      { state: "ended", reason: "closed", retry: true },
      { state: "connecting", attempt: 1 },
    ]),
  );
  // The reconnect attaches the same session again.
  const next = await socket(1);
  await next.serve([session("s", { status: "restored" })], { incarnation: "j", gap: true });
  bootstrap(next, await next.opened("s"), session("s", { status: "restored" }));
  await vi.waitFor(() =>
    expect(worker.links().at(-1)).toEqual({ state: "connected", session: session("s", { status: "restored" }) }),
  );
});

test("the link: an answered connection's error ends it, and nothing reconnects until a terminal attaches", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s")]);
  bootstrap(server, await server.opened("s"), session("s"));
  await vi.waitFor(() => expect(worker.links().at(-1)).toEqual({ state: "connected", session: session("s") }));
  server.push(FrameType.Error, 0, { code: "protocol_error", message: "bad frame" });
  await vi.waitFor(() =>
    expect(worker.links().at(-1)).toEqual({
      state: "ended",
      reason: "error",
      code: "protocol_error",
      message: "bad frame",
      retry: false,
    }),
  );
  // Past the first backoff: no new connection, no new link.
  await new Promise((resolve) => setTimeout(resolve, 400));
  expect(FakeSocket.all).toHaveLength(1);
  expect(worker.links().at(-1)?.state).toBe("ended");
  // Until the page attaches: that connects again.
  worker.to(1, { type: "attach", session: "s" });
  const next = await socket(1);
  await next.serve([session("s")]);
  bootstrap(next, await next.opened("s"), session("s"));
  await vi.waitFor(() =>
    expect(worker.links().slice(-2)).toEqual<Link[]>([
      { state: "connecting", attempt: 1 },
      { state: "connected", session: session("s") },
    ]),
  );
});

test("the link: a refused subscription closes the connection for good", async () => {
  const worker = await start();
  worker.mount(1);
  const server = await socket(0);
  await server.welcome();
  const subscribe = await server.request("subscribe");
  server.push(FrameType.Error, 0, { id: subscribe.id, code: "internal", message: "no" });
  await vi.waitFor(() =>
    expect(worker.links().at(-1)).toEqual({
      state: "ended",
      reason: "error",
      code: "internal",
      message: "no",
      retry: false,
    }),
  );
  await new Promise((resolve) => setTimeout(resolve, 400));
  expect(FakeSocket.all).toHaveLength(1);
  expect(worker.links().at(-1)?.state).toBe("ended");
});

test("the link: a resume onto a session that exited meanwhile ends once, at its EXITED", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s")]);
  bootstrap(server, await server.opened("s"), session("s"));
  await vi.waitFor(() => expect(worker.links().at(-1)).toEqual({ state: "connected", session: session("s") }));
  server.close();
  const next = await socket(1);
  const exited = { ...session("s", { status: "exited" }), exit: { code: 0 } };
  await next.serve([exited]);
  const channel = await next.opened("s");
  expect(next.bodies(FrameType.Open).at(-1)?.body.resume).toEqual({ incarnation: "i", offset: 0, marker: 0 });
  next.push(FrameType.Attached, channel, { mode: "resume", offset: 0, marker: 0, cols: 10, rows: 3, session: exited });
  next.push(FrameType.Exited, channel, { seq: 1, offset: 0, exit: { code: 0 } });
  await vi.waitFor(() =>
    expect(worker.links().slice(-2)).toEqual<Link[]>([
      { state: "connected", session: exited },
      { state: "ended", reason: "exited", exit: { code: 0 }, retry: false },
    ]),
  );
  expect(worker.links().filter((l) => l.state === "ended" && l.reason === "exited")).toHaveLength(1);
});

test("the link: a server refusing the handshake ends it; a lost one is tried again", async () => {
  const refused = await start();
  refused.mount(1);
  const server = await socket(0);
  await vi.waitFor(() => expect(server.bodies(FrameType.Hello)).toHaveLength(1));
  server.push(FrameType.Error, 0, { code: "version_mismatch", message: "version 2 only" });
  await vi.waitFor(() =>
    expect(refused.links()).toEqual<Link[]>([
      { state: "connecting", attempt: 1 },
      { state: "ended", reason: "error", code: "version_mismatch", message: "version 2 only", retry: false },
    ]),
  );

  const lost = await start();
  lost.mount(1);
  const first = await socket(1);
  await vi.waitFor(() => expect(first.bodies(FrameType.Hello)).toHaveLength(1));
  first.close();
  await vi.waitFor(() =>
    expect(lost.links()).toEqual<Link[]>([
      { state: "connecting", attempt: 1 },
      { state: "connecting", attempt: 2 },
    ]),
  );
  await (await socket(2)).serve();
  await vi.waitFor(() => expect(lost.links().at(-1)).toEqual({ state: "connected", session: undefined }));
});

test("the HELLO names the client the page gave", async () => {
  run().send({ type: "init", wasm: "w", url: "wss://127.0.0.1:7438/", client: { name: "clankerspace", version: "9" } });
  const server = await socket(0);
  await vi.waitFor(() => expect(server.bodies(FrameType.Hello).at(0)?.body.client).toEqual({ name: "clankerspace", version: "9" }));
});

test("an engine that does not load fails the client", async () => {
  vi.stubGlobal("fetch", async () => ({ arrayBuffer: async () => new ArrayBuffer(8) }));
  const { posted, send } = run();
  send({ type: "init", wasm: "w", url: "wss://127.0.0.1:7438/", client: undefined });
  await vi.waitFor(() => expect(posted.at(0)?.type).toBe("failed"));
  expect(FakeSocket.all).toEqual([]);
});

test("the wheel and Shift+PageUp scroll the history; input goes back to the live screen", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"), "l1\r\nl2\r\nl3\r\nl4\r\nl5\r\nl6");
  await vi.waitFor(() => expect(worker.scrollbar()).toEqual({ total: 6, offset: 3, len: 3 }));
  const inputs = () => server.sent.filter((f) => f.header.type === FrameType.Input).map((f) => new TextDecoder().decode(f.payload));

  worker.to(1, { type: "wheel", rows: -1 });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 2, len: 3 });
  worker.to(1, { type: "key", input: key("PageUp", { shift: true }) });
  worker.to(1, { type: "key", input: key("PageUp", { shift: true, action: "release" }) });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 0, len: 3 });
  worker.to(1, { type: "key", input: key("End", { shift: true }) });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 3, len: 3 });
  worker.to(1, { type: "key", input: key("Home", { shift: true }) });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 0, len: 3 });
  // A modifier alone encodes to nothing and stays where it is.
  worker.to(1, { type: "key", input: key("ShiftLeft", { key: "Shift", shift: true }) });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 0, len: 3 });

  // The page's own scrolling: by rows, and back to the live screen.
  worker.to(1, { type: "view", rows: 2 });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 2, len: 3 });
  worker.to(1, { type: "view", rows: "live" });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 3, len: 3 });
  worker.to(1, { type: "view", rows: -3 });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 0, len: 3 });

  // Output while scrolled back leaves the view where it is.
  server.pushRaw(encodeFrame(FrameType.Output, channel, output(0, "\r\nl7")));
  await vi.waitFor(() => expect(worker.scrollbar()).toEqual({ total: 7, offset: 0, len: 3 }));
  expect(inputs()).toEqual([]);

  // Typing goes back to the live screen, and is sent; so does composed
  // text, as UTF-8.
  worker.to(1, { type: "key", input: key("KeyA", { key: "a" }) });
  expect(worker.scrollbar()).toEqual({ total: 7, offset: 4, len: 3 });
  worker.to(1, { type: "wheel", rows: -1 });
  worker.to(1, { type: "text", text: "日本" });
  expect(worker.scrollbar()).toEqual({ total: 7, offset: 4, len: 3 });
  await vi.waitFor(() => expect(inputs()).toEqual(["a", "日本"]));
});

test("on the alternate screen the wheel is arrow keys and Shift+PageUp is a key", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"), "l1\r\nl2\r\nl3\r\nl4\x1b[?1049h");
  await vi.waitFor(() => expect(worker.scrollbar()).toEqual({ total: 3, offset: 0, len: 3 }));
  const inputs = () => server.sent.filter((f) => f.header.type === FrameType.Input).map((f) => new TextDecoder().decode(f.payload));

  worker.to(1, { type: "wheel", rows: -2 });
  worker.to(1, { type: "wheel", rows: 1 });
  worker.to(1, { type: "key", input: key("PageUp", { shift: true }) });
  await vi.waitFor(() => expect(inputs()).toEqual(["\x1b[A\x1b[A", "\x1b[B", "\x1b[5;2~"]));
  expect(worker.scrollbar()).toEqual({ total: 3, offset: 0, len: 3 });

  // Without alternate scroll (mode 1007) the wheel does nothing.
  server.pushRaw(encodeFrame(FrameType.Output, channel, output(0, "\x1b[?1007l")));
  worker.to(1, { type: "wheel", rows: -2 });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(inputs()).toHaveLength(3);
});

test("a drag selects and copies on release; a click, input or a new screen clears it", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"), "one two\r\nthree four five");
  await vi.waitFor(() => expect(worker.scrollbar()).toBeDefined());
  // A release is always answered by its number, with undefined when
  // nothing is selected.
  const selections = () => worker.selections().filter((m) => m.text !== undefined || m.copy === undefined);
  const answer = () => worker.selections().at(-1);
  const selectedRows = () =>
    worker.posted.flatMap((m) => (m.type === "frame" ? [m.frame.lines.map((l) => l.selection)] : [])).at(-1);

  worker.to(1, { type: "select", action: "press", at: { x: 4.2, y: 0.5 }, time: 1000 });
  worker.to(1, { type: "select", action: "drag", at: { x: 2.6, y: 1.5 }, time: 0 });
  expect(selectedRows()).toEqual([[4, 9], [0, 2], undefined]);
  expect(selections()).toEqual([]);
  worker.to(1, { type: "select", action: "release", at: { x: 2.6, y: 1.5 }, time: 1100, copy: 1 });
  expect(selections()).toEqual([{ type: "selection", mount: 1, text: "two\nthr", copy: 1 }]);

  // A key clears it, and the page forgets the text.
  worker.to(1, { type: "key", input: key("KeyA", { key: "a" }) });
  expect(selections().at(-1)).toEqual({ type: "selection", mount: 1, text: undefined });
  expect(selectedRows()).toEqual([undefined, undefined, undefined]);

  // A double click selects the word at once; a single click clears it.
  worker.to(1, { type: "select", action: "press", at: { x: 1, y: 1.5 }, time: 5000 });
  worker.to(1, { type: "select", action: "release", at: { x: 1, y: 1.5 }, time: 5050, copy: 2 });
  worker.to(1, { type: "select", action: "press", at: { x: 1, y: 1.5 }, time: 5100 });
  worker.to(1, { type: "select", action: "release", at: { x: 1, y: 1.5 }, time: 5150, copy: 3 });
  expect(selections().at(-1)).toEqual({ type: "selection", mount: 1, text: "three", copy: 3 });
  worker.to(1, { type: "select", action: "press", at: { x: 1, y: 0.5 }, time: 9000 });
  expect(selections().at(-1)).toEqual({ type: "selection", mount: 1, text: undefined });
  worker.to(1, { type: "select", action: "release", at: { x: 1, y: 0.5 }, time: 9050, copy: 4 });
  expect(selections()).toHaveLength(4);
  expect(answer()).toEqual({ type: "selection", mount: 1, text: undefined, copy: 4 });

  // Selected again, then a new screen: gone.
  worker.to(1, { type: "select", action: "press", at: { x: 0, y: 0.5 }, time: 20000 });
  worker.to(1, { type: "select", action: "drag", at: { x: 5, y: 0.5 }, time: 0 });
  worker.to(1, { type: "select", action: "release", at: { x: 5, y: 0.5 }, time: 20100, copy: 5 });
  expect(selections().at(-1)).toEqual({ type: "selection", mount: 1, text: "one t", copy: 5 });
  server.push(FrameType.Gap, channel, { from: 0, to: 10 });
  expect(selections().at(-1)).toEqual({ type: "selection", mount: 1, text: undefined });

  // A press, then a new screen: the drag that follows is not anchored in
  // the freed terminal, and selects nothing until a new press.
  worker.to(1, { type: "select", action: "press", at: { x: 0, y: 0.5 }, time: 30000 });
  bootstrap(server, channel, session("s"), "new screen");
  await vi.waitFor(() => expect(worker.links().filter((l) => l.state === "connected")).toHaveLength(2));
  worker.to(1, { type: "select", action: "drag", at: { x: 5, y: 0.5 }, time: 0 });
  worker.to(1, { type: "select", action: "release", at: { x: 5, y: 0.5 }, time: 30100, copy: 6 });
  expect(answer()).toEqual({ type: "selection", mount: 1, text: undefined, copy: 6 });
  worker.to(1, { type: "select", action: "press", at: { x: 0, y: 0.5 }, time: 40000 });
  worker.to(1, { type: "select", action: "drag", at: { x: 5, y: 0.5 }, time: 0 });
  worker.to(1, { type: "select", action: "release", at: { x: 5, y: 0.5 }, time: 40100, copy: 7 });
  expect(answer()).toEqual({ type: "selection", mount: 1, text: "new s", copy: 7 });

  // With no terminal (detached, or before its screen) a release is
  // answered too.
  worker.to(1, { type: "detach" });
  worker.to(1, { type: "select", action: "press", at: { x: 0, y: 0.5 }, time: 50000 });
  worker.to(1, { type: "select", action: "release", at: { x: 5, y: 0.5 }, time: 50100, copy: 8 });
  expect(answer()).toEqual({ type: "selection", mount: 1, text: undefined, copy: 8 });
});

test("a page still in use takes the size back at its next input after another client resized the session", async () => {
  const worker = await start();
  worker.mount(1, "s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"));
  await vi.waitFor(() => expect(worker.scrollbar()).toBeDefined());
  const size = { cols: 10, rows: 3 };
  worker.to(1, { type: "viewport", size, profile: {}, active: true });
  expect(server.bodies(FrameType.Viewport)).toHaveLength(1);
  // Its own size coming back changes nothing.
  server.push(FrameType.Resized, channel, { seq: 1, offset: 0, cols: 10, rows: 3 });
  worker.to(1, { type: "key", input: key("KeyA", { key: "a" }) });
  await vi.waitFor(() => expect(server.sent.filter((f) => f.header.type === FrameType.Input)).toHaveLength(1));
  expect(server.bodies(FrameType.Viewport)).toHaveLength(1);
  // Another client's: the next input reports this page's size first, once.
  server.push(FrameType.Resized, channel, { seq: 2, offset: 0, cols: 20, rows: 5 });
  worker.to(1, { type: "wheel", rows: -1 });
  worker.to(1, { type: "key", input: key("KeyB", { key: "b" }) });
  expect(server.bodies(FrameType.Viewport)).toEqual([
    { channel, body: { ...size, profile: {} } },
    { channel, body: { ...size, profile: {} } },
  ]);
});
