/**
 * The worker as the page drives it, against a server played by the test:
 * the page's requests become the right protocol requests, their outcomes
 * come back by id, and the link reports what happened to the session.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";

import { Engine, type KeyInput } from "../../src/engine/index.ts";
import { type Frame, FrameType, type Limits, type Session, encodeFrame, parseFrame } from "../../src/proto/index.ts";

import { type Link, type ToPage, type ToWorker, runWorker } from "../../src/client/worker.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const wasm = readFileSync(`${here}../../ghostty-vt.wasm`);
const limits: Limits = JSON.parse(readFileSync(`${here}../../../../../crates/proto/vectors/limits.json`, "utf8"));
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

  /** Answers `HELLO` and the `subscribe` with `sessions`. */
  async serve(sessions: Session[] = []): Promise<void> {
    await vi.waitFor(() => expect(this.bodies(FrameType.Hello)).toHaveLength(1));
    this.push(FrameType.Welcome, 0, {
      version: 1,
      server: { name: "test", version: "0" },
      engine: "x",
      incarnation: "i",
      capabilities: [],
      limits,
    });
    const subscribe = await this.request("subscribe");
    this.push(FrameType.Reply, 0, { id: subscribe.id, result: { sessions } });
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

function session(id: string, status: Session["status"] = "running"): Session {
  return {
    id,
    label: "",
    status,
    argv: [],
    cwd: "",
    cols: 10,
    rows: 3,
    title: "",
    pwd: "",
    on_exit: "remove",
    created_at: "",
    offset: 0,
    marker: 0,
    attachments: 1,
  };
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

/** A worker started as `mountTerminal` starts it, and what it posted. */
function start(attach?: string, active = true) {
  const posted: ToPage[] = [];
  const port = {
    postMessage: (message: ToPage) => posted.push(message),
    onmessage: null as ((e: MessageEvent<ToWorker>) => void) | null,
  };
  runWorker(port);
  const send = (message: ToWorker) => port.onmessage!({ data: message } as MessageEvent<ToWorker>);
  send({
    type: "init",
    wasm: "ghostty-vt.wasm",
    url: "wss://127.0.0.1:7438/",
    viewport: { cols: 80, rows: 24 },
    profile: {},
    active,
    session: attach,
  });
  return {
    send,
    posted,
    /** The latest frame's scrollbar, once one was posted. */
    scrollbar: () => posted.flatMap((m) => (m.type === "frame" ? [m.frame.scrollbar] : [])).at(-1),
    links: () => posted.flatMap((m) => (m.type === "link" ? [m.link] : [])),
    async done(id: number) {
      let found: Extract<ToPage, { type: "done" }> | undefined;
      await vi.waitFor(() => {
        found = posted.find((m): m is Extract<ToPage, { type: "done" }> => m.type === "done" && m.id === id);
        expect(found).toBeDefined();
      });
      return found!;
    },
  };
}

/** `OUTPUT` of `text` at stream `offset`. */
function output(offset: number, text: string): Uint8Array {
  const bytes = new TextEncoder().encode(text);
  const payload = new Uint8Array(8 + bytes.length);
  new DataView(payload.buffer).setBigUint64(0, BigInt(offset), true);
  payload.set(bytes, 8);
  return payload;
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

test("create sends on_exit only when asked to, answers with the session and attaches it", async () => {
  const worker = start();
  const server = await socket(0);
  await server.serve();
  await vi.waitFor(() => expect(worker.links()).toContainEqual({ state: "connected", session: undefined }));

  worker.send({ type: "create", id: 1, onExit: "keep" });
  const keep = await server.request("create");
  expect(keep.params).toEqual({ cols: 80, rows: 24, on_exit: "keep" });
  server.push(FrameType.Reply, 0, { id: keep.id, result: session("a") });
  expect(await worker.done(1)).toEqual({ type: "done", id: 1, session: session("a") });
  await server.opened("a");

  worker.send({ type: "create", id: 2 });
  const plain = await server.request("create", keep.id);
  expect(plain.params).toEqual({ cols: 80, rows: 24 });
  server.push(FrameType.Error, 0, { id: plain.id, code: "invalid_request", message: "no" });
  expect((await worker.done(2)).error).toEqual({
    kind: "server",
    message: "invalid_request: no",
    server: { id: plain.id, code: "invalid_request", message: "no" },
  });
});

test("end and remove name the attached session; not_running ends nothing and is no failure", async () => {
  const worker = start("s");
  const server = await socket(0);
  await server.serve([session("s")]);
  await server.opened("s");

  worker.send({ type: "end", id: 1 });
  const end = await server.request("end");
  expect(end.params).toEqual({ session: "s" });
  server.push(FrameType.Error, 0, { id: end.id, code: "not_running", message: "session is not running" });
  expect(await worker.done(1)).toEqual({ type: "done", id: 1 });

  worker.send({ type: "end", id: 2 });
  const failed = await server.request("end", end.id);
  server.push(FrameType.Error, 0, { id: failed.id, code: "signal_failed", message: "nobody" });
  expect((await worker.done(2)).error?.server?.code).toBe("signal_failed");

  worker.send({ type: "remove", id: 3 });
  const running = await server.request("remove");
  expect(running.params).toEqual({ session: "s" });
  server.push(FrameType.Error, 0, { id: running.id, code: "running", message: "session is running" });
  expect((await worker.done(3)).error?.server?.code).toBe("running");

  worker.send({ type: "remove", id: 4 });
  const removed = await server.request("remove", running.id);
  server.push(FrameType.Reply, 0, { id: removed.id, result: {} });
  expect(await worker.done(4)).toEqual({ type: "done", id: 4 });
});

test("a request with no session or no connection fails without asking the server", async () => {
  const worker = start();
  worker.send({ type: "end", id: 1 });
  expect((await worker.done(1)).error).toEqual({ kind: "closed", message: "not connected" });
  const server = await socket(0);
  await server.serve();
  await vi.waitFor(() => expect(worker.links()).toContainEqual({ state: "connected", session: undefined }));
  worker.send({ type: "remove", id: 2 });
  expect((await worker.done(2)).error).toEqual({ message: "no session is attached" });
  expect(server.bodies(FrameType.Request).map((r) => r.body.method)).toEqual(["subscribe"]);
});

test("the link: connected to a session, its exit, then removed for good", async () => {
  const worker = start("s");
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

test("only a page in use reports its size: OPEN and VIEWPORT carry none until it is active", async () => {
  const worker = start("s", false);
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  expect(server.bodies(FrameType.Open).at(-1)!.body).toEqual({ session: "s" });
  bootstrap(server, channel, session("s"));
  await vi.waitFor(() => expect(worker.links()).toContainEqual({ state: "connected", session: session("s") }));

  const size = { cols: 100, rows: 30 };
  worker.send({ type: "viewport", size, profile: { scheme: "dark" }, active: false });
  expect(server.bodies(FrameType.Viewport)).toEqual([]);
  // Focused again with the same size: reported, to take the size back.
  worker.send({ type: "viewport", size, profile: { scheme: "dark" }, active: true });
  expect(server.bodies(FrameType.Viewport)).toEqual([{ channel, body: { ...size, profile: { scheme: "dark" } } }]);
  // A size made while blurred is the one `create` asks for.
  worker.send({ type: "viewport", size: { cols: 50, rows: 10 }, profile: {}, active: false });
  expect(server.bodies(FrameType.Viewport)).toHaveLength(1);
  worker.send({ type: "create", id: 1 });
  expect((await server.request("create")).params).toEqual({ cols: 50, rows: 10 });
});

test("a program's title and working directory reach the page as they change", async () => {
  const worker = start("s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const changed = { ...session("s"), title: "vim notes.md", pwd: "/tmp/work" };
  server.push(FrameType.Event, 0, { event: "session_changed", session: changed });
  await vi.waitFor(() =>
    expect(worker.posted.at(-1)).toEqual({ type: "sessions", sessions: [changed], current: "s" }),
  );
});

test("the link: an exited or refused session ends it, a server stopping is retried", async () => {
  const worker = start("old");
  const server = await socket(0);
  await server.serve([session("old", "exited")]);
  const exited = { ...session("old", "exited"), exit: { signal: "HUP" } };
  bootstrap(server, await server.opened("old"), exited);
  await vi.waitFor(() =>
    expect(worker.links().slice(1)).toEqual<Link[]>([
      { state: "connected", session: exited },
      { state: "ended", reason: "exited", exit: { signal: "HUP" }, retry: false },
    ]),
  );

  worker.send({ type: "attach", session: "gone" });
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

  worker.send({ type: "attach", session: "s" });
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
  await next.serve([session("s", "restored")]);
  bootstrap(next, await next.opened("s"), session("s", "restored"));
  await vi.waitFor(() =>
    expect(worker.links().at(-1)).toEqual({ state: "connected", session: session("s", "restored") }),
  );
});

test("the link: an answered connection's error ends it, and nothing reconnects", async () => {
  const worker = start("s");
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
  worker.send({ type: "attach", session: "s" });
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
  const worker = start();
  const server = await socket(0);
  await vi.waitFor(() => expect(server.bodies(FrameType.Hello)).toHaveLength(1));
  server.push(FrameType.Welcome, 0, {
    version: 1,
    server: { name: "test", version: "0" },
    engine: "x",
    incarnation: "i",
    capabilities: [],
    limits,
  });
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
  const worker = start("s");
  const server = await socket(0);
  await server.serve([session("s")]);
  bootstrap(server, await server.opened("s"), session("s"));
  await vi.waitFor(() => expect(worker.links().at(-1)).toEqual({ state: "connected", session: session("s") }));
  server.close();
  const next = await socket(1);
  const exited = { ...session("s", "exited"), exit: { code: 0 } };
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
  const refused = start();
  const server = await socket(0);
  await vi.waitFor(() => expect(server.bodies(FrameType.Hello)).toHaveLength(1));
  server.push(FrameType.Error, 0, { code: "version_mismatch", message: "version 2 only" });
  await vi.waitFor(() =>
    expect(refused.links()).toEqual<Link[]>([
      { state: "connecting", attempt: 1 },
      { state: "ended", reason: "error", code: "version_mismatch", message: "version 2 only", retry: false },
    ]),
  );

  const lost = start();
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

test("the wheel and Shift+PageUp scroll the history; input goes back to the live screen", async () => {
  const worker = start("s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"), "l1\r\nl2\r\nl3\r\nl4\r\nl5\r\nl6");
  await vi.waitFor(() => expect(worker.scrollbar()).toEqual({ total: 6, offset: 3, len: 3 }));
  const inputs = () => server.sent.filter((f) => f.header.type === FrameType.Input).map((f) => new TextDecoder().decode(f.payload));

  worker.send({ type: "scroll", rows: -1 });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 2, len: 3 });
  worker.send({ type: "key", input: key("PageUp", { shift: true }) });
  worker.send({ type: "key", input: key("PageUp", { shift: true, action: "release" }) });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 0, len: 3 });
  worker.send({ type: "key", input: key("End", { shift: true }) });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 3, len: 3 });
  worker.send({ type: "key", input: key("Home", { shift: true }) });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 0, len: 3 });
  // A modifier alone encodes to nothing and stays where it is.
  worker.send({ type: "key", input: key("ShiftLeft", { key: "Shift", shift: true }) });
  expect(worker.scrollbar()).toEqual({ total: 6, offset: 0, len: 3 });

  // Output while scrolled back leaves the view where it is.
  server.pushRaw(encodeFrame(FrameType.Output, channel, output(0, "\r\nl7")));
  await vi.waitFor(() => expect(worker.scrollbar()).toEqual({ total: 7, offset: 0, len: 3 }));
  expect(inputs()).toEqual([]);

  // Typing goes back to the live screen, and is sent.
  worker.send({ type: "key", input: key("KeyA", { key: "a" }) });
  expect(worker.scrollbar()).toEqual({ total: 7, offset: 4, len: 3 });
  await vi.waitFor(() => expect(inputs()).toEqual(["a"]));
});

test("on the alternate screen the wheel is arrow keys and Shift+PageUp is a key", async () => {
  const worker = start("s");
  const server = await socket(0);
  await server.serve([session("s")]);
  const channel = await server.opened("s");
  bootstrap(server, channel, session("s"), "l1\r\nl2\r\nl3\r\nl4\x1b[?1049h");
  await vi.waitFor(() => expect(worker.scrollbar()).toEqual({ total: 3, offset: 0, len: 3 }));
  const inputs = () => server.sent.filter((f) => f.header.type === FrameType.Input).map((f) => new TextDecoder().decode(f.payload));

  worker.send({ type: "scroll", rows: -2 });
  worker.send({ type: "scroll", rows: 1 });
  worker.send({ type: "key", input: key("PageUp", { shift: true }) });
  await vi.waitFor(() => expect(inputs()).toEqual(["\x1b[A\x1b[A", "\x1b[B", "\x1b[5;2~"]));
  expect(worker.scrollbar()).toEqual({ total: 3, offset: 0, len: 3 });

  // Without alternate scroll (mode 1007) the wheel does nothing.
  server.pushRaw(encodeFrame(FrameType.Output, channel, output(0, "\x1b[?1007l")));
  worker.send({ type: "scroll", rows: -2 });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(inputs()).toHaveLength(3);
});
