/**
 * The client against a real server, from node: over the Unix socket, where
 * the test reframes the byte stream itself, and over the TLS WebSocket
 * carrier the browser uses, one frame per message.
 */
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { connect as netConnect, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, expect, test } from "vitest";
import WebSocket from "ws";

import { ENGINE, Engine } from "@clankermux/engine";
import { type Event, type Frame, FrameType, type Read, type Session, StreamFramer } from "@clankermux/proto";

import { type Action, Attachment, Connection, type Transport, hello } from "../src/index.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const root = `${here}../../../../`;
const binary = `${root}target/debug/clankermux`;
const built = existsSync(binary);

/** Frames over a stream socket, one `onMessage` per frame. */
class StreamTransport implements Transport {
  private readonly framer = new StreamFramer(1 << 30);
  onMessage: ((bytes: Uint8Array) => void) | undefined;
  onClose: (() => void) | undefined;

  constructor(private readonly socket: Socket) {
    socket.on("data", (chunk: Buffer) => {
      for (const frame of this.framer.feed(chunk)) this.onMessage?.(frame);
    });
    socket.on("close", () => this.onClose?.());
    socket.on("error", () => {});
  }

  static connect(path: string): Promise<StreamTransport> {
    return new Promise((resolve, reject) => {
      const socket = netConnect(path);
      socket.once("connect", () => resolve(new StreamTransport(socket)));
      socket.once("error", reject);
    });
  }

  send(bytes: Uint8Array): void {
    this.socket.write(bytes);
  }

  close(): void {
    this.socket.destroy();
  }
}

/** Frames over a WebSocket, as the browser carries them. */
class WsTransport implements Transport {
  onMessage: ((bytes: Uint8Array) => void) | undefined;
  onClose: (() => void) | undefined;

  constructor(private readonly ws: WebSocket) {
    ws.on("message", (data: Buffer) => this.onMessage?.(new Uint8Array(data)));
    ws.on("close", () => this.onClose?.());
    ws.on("error", () => {});
  }

  static connect(url: string, origin: string): Promise<WsTransport> {
    return new Promise((resolve, reject) => {
      // The fixture certificate is self-signed.
      const ws = new WebSocket(url, { origin, rejectUnauthorized: false });
      ws.once("open", () => resolve(new WsTransport(ws)));
      ws.once("error", reject);
    });
  }

  send(bytes: Uint8Array): void {
    this.ws.send(bytes);
  }

  close(): void {
    this.ws.close();
  }
}

let dir: string;
let socket: string;
let wsUrl: string;
let server: ReturnType<typeof spawn>;
let engine: Engine;

const ORIGIN = "http://page.test";

beforeAll(async () => {
  if (!built) return;
  engine = await Engine.load(readFileSync(`${here}../../engine/ghostty-vt.wasm`));
  dir = mkdtempSync(join(tmpdir(), "cmx-"));
  socket = join(dir, "sock");
  const tls = `${here}fixtures/tls/`;
  server = spawn(
    binary,
    ["--socket", socket, "serve", "--ws", "127.0.0.1:0", "--origin", ORIGIN, "--tls-cert", `${tls}cert.pem`, "--tls-key", `${tls}key.pem`],
    { stdio: ["ignore", "ignore", "pipe"], env: { ...process.env, XDG_STATE_HOME: join(dir, "state") } },
  );
  // The bound port is announced on stderr.
  wsUrl = await new Promise((resolve, reject) => {
    let seen = "";
    server.stderr!.on("data", (chunk: Buffer) => {
      process.stderr.write(chunk);
      seen += chunk;
      const m = /listening on (wss:\/\/\S+)/.exec(seen);
      if (m) resolve(m[1]!);
    });
    server.once("exit", (code) => reject(new Error(`server exited: ${code}`)));
  });
  const deadline = Date.now() + 10_000;
  for (;;) {
    try {
      (await StreamTransport.connect(socket)).close();
      return;
    } catch {
      if (Date.now() > deadline) throw new Error("server did not listen");
      await new Promise((r) => setTimeout(r, 20));
    }
  }
});

afterAll(async () => {
  // A graceful stop parks into the state directory; wait before removing it.
  if (server && server.exitCode === null) await new Promise((resolve) => (server.once("exit", resolve), server.kill()));
  if (dir) rmSync(dir, { recursive: true, force: true });
});

async function connect(): Promise<Connection> {
  return Connection.connect(await StreamTransport.connect(socket), hello("test", ENGINE));
}

/** Opens channel 1 and queues its frames. */
function attach(conn: Connection, a: Attachment, open: unknown): { frames: Frame[]; wake: () => Promise<void> } {
  const frames: Frame[] = [];
  let notify: (() => void) | undefined;
  a.openOn(conn.welcome);
  conn.open(1, open, (f) => {
    frames.push(f);
    notify?.();
  });
  return {
    frames,
    wake: () =>
      new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("no frame within 5 s")), 5000);
        notify = () => {
          clearTimeout(timer);
          resolve();
        };
      }),
  };
}

/** Feeds queued frames until `done`, returning every action. */
async function drive(
  a: Attachment,
  queue: { frames: Frame[]; wake: () => Promise<void> },
  done: (actions: Action[]) => boolean,
): Promise<Action[]> {
  const all: Action[] = [];
  for (;;) {
    while (queue.frames.length > 0) {
      all.push(...a.handle(queue.frames.shift()!));
      if (done(all)) return all;
    }
    await queue.wake();
  }
}

const screen = (a: Attachment) => a.term()!.formatPlain().trimEnd();

test.skipIf(!built)("bootstraps, streams, resumes and sees the exit", async () => {
  const conn = await connect();
  expect(conn.welcome.engine).toBe(ENGINE);
  const session = await conn.call<Session>("create", { argv: ["/bin/cat"], cols: 20, rows: 5 });
  await conn.call("write", { session: session.id, data: btoa("one\n") });
  for (const deadline = Date.now() + 5000; ; ) {
    const read = await conn.call<Read>("read", { session: session.id });
    if (read.text.includes("one\none")) break;
    if (Date.now() > deadline) throw new Error(`echo not seen: ${JSON.stringify(read.text)}`);
    await new Promise((r) => setTimeout(r, 10));
  }

  const a = new Attachment(engine);
  const open = { session: session.id, viewport: { cols: 20, rows: 5 } };
  let queue = attach(conn, a, open);
  let actions = await drive(a, queue, () => a.resume() !== undefined);
  expect(actions).toEqual([{ type: "screen" }]);
  expect(screen(a)).toBe("one\none");
  expect(a.session!.id).toBe(session.id);

  conn.send(FrameType.Input, 1, new TextEncoder().encode("two\n"));
  await drive(a, queue, () => screen(a).includes("two\ntwo"));
  const cursor = a.resume()!;
  expect(cursor.offset).toBeGreaterThan(0);

  // The connection drops; a new one resumes from the cursor.
  conn.close();
  const again = await connect();
  queue = attach(again, a, { ...open, resume: cursor });
  actions = await drive(a, queue, () => a.resume() !== undefined);
  expect(actions).toEqual([]);
  expect(screen(a)).toBe("one\none\ntwo\ntwo");

  // A viewport report resizes; a kill ends the stream and the events say so.
  const events: Event[] = [];
  again.onEvent = (e) => events.push(e);
  await again.call("subscribe", {});
  again.sendJson(FrameType.Viewport, 1, { cols: 30, rows: 6 });
  actions = await drive(a, queue, (all) => all.some((x) => x.type === "resized"));
  expect(actions).toContainEqual({ type: "resized", cols: 30, rows: 6 });
  expect(a.term()!.cols).toBe(30);
  await again.call("signal", { session: session.id, signal: "KILL" });
  actions = await drive(a, queue, () => a.ended());
  expect(actions.at(-1)).toEqual({ type: "exited", exit: { signal: "KILL" } });
  for (let i = 0; i < 100 && !events.some((e) => e.event === "session_exited"); i++) {
    await new Promise((r) => setTimeout(r, 10));
  }
  expect(events.some((e) => e.event === "session_exited" && e.session.id === session.id)).toBe(true);

  // Server errors arrive typed; a second HELLO ends the connection.
  await expect(again.call("inspect", { session: "nope" })).rejects.toMatchObject({
    server: { code: "no_such_session" },
  });
  const ended = new Promise((resolve) => (again.onEnd = resolve));
  again.sendJson(FrameType.Hello, 0, hello("test"));
  await expect(ended).resolves.toMatchObject({ kind: "server", server: { code: "protocol_error" } });
  a.free();
});

test.skipIf(!built)("the TLS WebSocket carrier checks the origin and carries one frame per message", async () => {
  await expect(WsTransport.connect(wsUrl, "http://elsewhere.test")).rejects.toThrow(/403/);
  const conn = await Connection.connect(await WsTransport.connect(wsUrl, ORIGIN), hello("test", ENGINE));
  expect(conn.welcome.engine).toBe(ENGINE);
  const session = await conn.call<Session>("create", { argv: ["/bin/cat"], cols: 20, rows: 5 });
  const a = new Attachment(engine);
  const queue = attach(conn, a, { session: session.id, viewport: { cols: 20, rows: 5 } });
  await drive(a, queue, (actions) => actions.some((x) => x.type === "screen"));
  conn.send(FrameType.Input, 1, new TextEncoder().encode("over wss\n"));
  await drive(a, queue, () => screen(a).includes("over wss\nover wss"));
  const read = await conn.call<Read>("read", { session: session.id });
  expect(read.text).toContain("over wss");
  conn.close();
}, 20_000);
