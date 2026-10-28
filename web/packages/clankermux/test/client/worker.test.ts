import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";

import { Engine, type KeyInput, type PackedFrame, RenderState } from "../../src/engine/index.ts";
import { PACKED_STRIDE, packedText } from "../../src/engine/render.ts";
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

  bodies(type: FrameType): { channel: number; body: Record<string, unknown> }[] {
    return this.sent
      .filter((f) => f.header.type === type)
      .map((f) => ({ channel: f.header.channel, body: JSON.parse(new TextDecoder().decode(f.payload)) }));
  }

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

  async welcome(incarnation = "i"): Promise<void> {
    await vi.waitFor(() => expect(this.bodies(FrameType.Hello)).toHaveLength(1));
    this.push(FrameType.Welcome, 0, welcome(incarnation));
  }

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

function run(autoPaint = true) {
  const posted: ToPage[] = [];
  const port = {
    postMessage: (message: ToPage, options?: StructuredSerializeOptions) => {
      posted.push(structuredClone(message, options));
      if (autoPaint && message.type === "frame") {
        port.onmessage?.({ data: { type: "painted", mount: message.mount } } as MessageEvent<ToWorker>);
      }
    },
    onmessage: null as ((e: MessageEvent<ToWorker>) => void) | null,
  };
  runWorker(port);
  const send = (message: ToWorker) => port.onmessage!({ data: message } as MessageEvent<ToWorker>);
  return { posted, send };
}

async function start(autoPaint = true) {
  const { posted, send } = run(autoPaint);
  send({ type: "init", wasm: "ghostty-vt.wasm", url: "wss://127.0.0.1:7438/", client: undefined });
  await vi.waitFor(() => expect(posted).toContainEqual({ type: "ready" }), { timeout: 5000 });
  let nextId = 100;
  const of = <T extends ToPage["type"]>(type: T) => posted.filter((m): m is Message<T> => m.type === type);
  return {
    send,
    posted,
    mount(mount: number, session?: string, extra: { active?: boolean; visible?: boolean; readOnly?: boolean; ackOnFocus?: boolean } = {}) {
      send({
        type: "mount",
        mount,
        session,
        viewport: { cols: 80, rows: 24 },
        profile: {},
        active: extra.active ?? true,
        visible: extra.visible ?? true,
        readOnly: extra.readOnly ?? false,
        ackOnFocus: extra.ackOnFocus ?? false,
      });
    },
    to(mount: number, message: ToMount) {
      send({ ...message, mount } as ToWorker);
    },
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
    scrollbar: (mount = 1) => of("frame").filter((m) => m.mount === mount).at(-1)?.frame.scrollbar,
    links: (mount = 1) => of("link").flatMap((m) => (m.mount === mount ? [m.link] : [])),
    screens: (mount = 1) => of("status").flatMap((m) => (m.mount === mount ? [m.text] : [])),
    selections: (mount = 1) => of("selection").filter((m) => m.mount === mount),
    events: () => of("event").map((m) => m.event),
    sessions: () => of("sessions").at(-1)?.sessions,
  };
}

function key(code: string, extra: Partial<KeyInput> = {}): KeyInput {
  return { code, key: code, action: "press", shift: false, ctrl: false, alt: false, meta: false, ...extra };
}

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
  server.push(FrameType.Event, 0, { seq: 2, event: "session_changed", session: done });
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(acks()).toHaveLength(1);
  const first = await server.request("ack");
  server.push(FrameType.Reply, 0, { id: first.id, result: { record_ids: [""] } });

  const blocked = { ...session("s"), attention: { state: "blocked", record: "q" } };
  server.push(FrameType.Event, 0, { seq: 3, event: "session_changed", session: blocked });
  const error = { ...session("s"), attention: { state: "error", record: "e" } };
  server.push(FrameType.Event, 0, { seq: 4, event: "session_changed", session: error });
  await vi.waitFor(() => expect(acks()).toEqual([{ session: "s" }, { session: "s" }]));

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
  await new Promise((resolve) => setTimeout(resolve, 400));
  expect(FakeSocket.all).toHaveLength(1);
  expect(worker.links().at(-1)?.state).toBe("ended");
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

test("the link: a connection lost while subscribing is retried, not refused", async () => {
  const worker = await start();
  worker.mount(1);
  const server = await socket(0);
  await server.welcome();
  await server.request("subscribe");
  server.close();
  await vi.waitFor(() => expect(worker.links().at(-1)).toEqual({ state: "connecting", attempt: 1 }));
  expect(worker.links()).not.toContainEqual(expect.objectContaining({ retry: false }));
  await (await socket(1)).serve();
  await vi.waitFor(() => expect(worker.links().at(-1)).toEqual({ state: "connected", session: undefined }));
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
  worker.to(1, { type: "key", input: key("ShiftLeft", { key: "Shift", shift: true }) });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 0, len: 3 });

  worker.to(1, { type: "view", rows: 2 });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 2, len: 3 });
  worker.to(1, { type: "view", rows: "live" });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 3, len: 3 });
  worker.to(1, { type: "view", rows: -3 });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 0, len: 3 });

  server.pushRaw(encodeFrame(FrameType.Output, channel, output(0, "\r\nl7")));
  await vi.waitFor(() => expect(worker.scrollbar()).toEqual({ total: 7, offset: 0, len: 3 }));
  expect(inputs()).toEqual([]);

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

  worker.to(1, { type: "key", input: key("KeyA", { key: "a" }) });
  expect(selections().at(-1)).toEqual({ type: "selection", mount: 1, text: undefined });
  expect(selectedRows()).toEqual([undefined, undefined, undefined]);

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

  worker.to(1, { type: "select", action: "press", at: { x: 0, y: 0.5 }, time: 20000 });
  worker.to(1, { type: "select", action: "drag", at: { x: 5, y: 0.5 }, time: 0 });
  worker.to(1, { type: "select", action: "release", at: { x: 5, y: 0.5 }, time: 20100, copy: 5 });
  expect(selections().at(-1)).toEqual({ type: "selection", mount: 1, text: "one t", copy: 5 });
  server.push(FrameType.Gap, channel, { from: 0, to: 10 });
  expect(selections().at(-1)).toEqual({ type: "selection", mount: 1, text: undefined });

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
  server.push(FrameType.Resized, channel, { seq: 1, offset: 0, cols: 10, rows: 3 });
  worker.to(1, { type: "key", input: key("KeyA", { key: "a" }) });
  await vi.waitFor(() => expect(server.sent.filter((f) => f.header.type === FrameType.Input)).toHaveLength(1));
  expect(server.bodies(FrameType.Viewport)).toHaveLength(1);
  server.push(FrameType.Resized, channel, { seq: 2, offset: 0, cols: 20, rows: 5 });
  worker.to(1, { type: "wheel", rows: -1 });
  worker.to(1, { type: "key", input: key("KeyB", { key: "b" }) });
  expect(server.bodies(FrameType.Viewport)).toEqual([
    { channel, body: { ...size, profile: {} } },
    { channel, body: { ...size, profile: {} } },
  ]);
});

test("one frame per mount waits for paint while burst rows accumulate in the engine", async () => {
  const worker = await start(false);
  const server = await socket(0);
  await server.serve([session("s"), session("t")]);
  worker.mount(1, "s");
  worker.mount(2, "t");
  const channel = await server.opened("s");
  const other = await server.opened("t");
  const extracts = vi.spyOn(RenderState.prototype, "updatePacked");
  const frames = (mount = 1) => worker.posted.flatMap((m) => m.type === "frame" && m.mount === mount ? [m.frame] : []);
  const term = engine.terminalNew(10, 3);
  term.write(new TextEncoder().encode("hello"));
  try {
    bootstrap(server, channel, session("s"));
    bootstrap(server, other, session("t"));
    expect(extracts).toHaveBeenCalledTimes(2);
    let offset = 0;
    for (let i = 0; i < 40; i++) {
      const text = `\x1b[${i % 3 + 1};1Hrow-${i}`;
      term.write(new TextEncoder().encode(text));
      server.pushRaw(encodeFrame(FrameType.Output, channel, output(offset, text)));
      offset += new TextEncoder().encode(text).length;
      worker.to(1, { type: "viewport", size: { cols: 10, rows: 3 }, profile: {}, active: false });
    }
    expect(frames()).toHaveLength(1);
    expect(extracts).toHaveBeenCalledTimes(2);
    worker.to(2, { type: "painted" });
    worker.to(2, { type: "repaint" });
    expect(frames(2)).toHaveLength(2);
    expect(frames()).toHaveLength(1);
    worker.to(1, { type: "painted" });
    expect(frames()).toHaveLength(2);
    expect(frames()[1]!.full).toBe(false);
    const screen: PackedFrame["lines"] = [];
    for (const frame of frames()) for (const row of frame.lines) screen[row.y] = row;
    const expected = new RenderState(engine);
    const final = expected.updatePacked(term);
    expect(screen).toEqual(final.lines);
    expect(frames().at(-1)!.cursor).toEqual(final.cursor);
    expected.free();
    server.push(FrameType.Resized, channel, { seq: 1, offset, cols: 12, rows: 4 });
    term.resize(12, 4);
    expect(frames()).toHaveLength(2);
    worker.to(1, { type: "painted" });
    expect(frames()).toHaveLength(3);
    const resized = new RenderState(engine);
    expect(frames().at(-1)).toEqual(resized.updatePacked(term));
    resized.free();
    worker.to(1, { type: "painted" });
    expect(frames()).toHaveLength(3);
  } finally {
    extracts.mockRestore();
    term.free();
    worker.send({ type: "unmount", mount: 1 });
    worker.send({ type: "unmount", mount: 2 });
  }
});

test("output clears an invalidated selection while a frame is still waiting for paint", async () => {
  const worker = await start(false);
  const server = await socket(0);
  await server.serve([session("s")]);
  worker.mount(1, "s");
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"));
  worker.to(1, { type: "select", action: "press", at: { x: 0.2, y: 0.5 }, time: 1000 });
  worker.to(1, { type: "select", action: "drag", at: { x: 2.6, y: 0.5 }, time: 0 });
  worker.to(1, { type: "select", action: "release", at: { x: 2.6, y: 0.5 }, time: 1100, copy: 1 });
  expect(worker.selections().at(-1)?.text).toBe("hel");
  server.pushRaw(encodeFrame(FrameType.Output, channel, output(0, "\x1b[?1049h")));
  expect(worker.selections().at(-1)).toEqual({ type: "selection", mount: 1, text: undefined });
  expect(worker.posted.filter((m) => m.type === "frame")).toHaveLength(1);
  worker.send({ type: "unmount", mount: 1 });
});

test("hidden mounts skip extraction and return with a full frame even without focus", async () => {
  const worker = await start(false);
  const server = await socket(0);
  await server.serve([session("s")]);
  worker.mount(1, "s", { active: false, visible: false });
  const channel = await server.opened("s");
  const frames = () => worker.posted.flatMap((m) => m.type === "frame" ? [m.frame] : []);
  const extracts = vi.spyOn(RenderState.prototype, "updatePacked");
  try {
    bootstrap(server, channel, session("s"));
    worker.to(1, { type: "repaint" });
    expect(extracts).not.toHaveBeenCalled();
    worker.to(1, { type: "visibility", visible: true });
    expect(frames()).toHaveLength(1);
    expect(frames()[0]!.full).toBe(true);
    worker.to(1, { type: "visibility", visible: false });
    worker.to(1, { type: "painted" });
    server.pushRaw(encodeFrame(FrameType.Output, channel, output(0, "\r\nnew")));
    worker.to(1, { type: "repaint" });
    expect(extracts).toHaveBeenCalledTimes(1);
    expect(frames()).toHaveLength(1);
    worker.to(1, { type: "visibility", visible: true });
    expect(frames()).toHaveLength(2);
    expect(frames()[1]!.full).toBe(true);
    const cells = frames()[1]!.lines[1]!.cells;
    expect(Array.from({ length: cells.data.length / PACKED_STRIDE }, (_, x) => packedText(cells, x)).join("")).toBe("new");
    worker.to(1, { type: "painted" });
    worker.to(1, { type: "viewport", size: { cols: 10, rows: 3 }, profile: {}, active: false });
    expect(frames()).toHaveLength(3);
    expect(server.bodies(FrameType.Viewport)).toHaveLength(0);
  } finally {
    extracts.mockRestore();
    worker.send({ type: "unmount", mount: 1 });
  }
});
