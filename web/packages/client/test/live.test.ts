/**
 * The client against a real server over the Unix socket, from node. The
 * browser has no stream sockets, so the test reframes the byte stream
 * itself; the message transport the browser uses is M6's carrier.
 */
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { connect as netConnect, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, expect, test } from "vitest";

import { ENGINE, Engine } from "@clankermux/engine";
import {
  type Event,
  type Frame,
  FrameType,
  HEADER_LEN,
  type Read,
  type Session,
  parseHeader,
} from "@clankermux/proto";

import { type Action, Attachment, Connection, type Transport, hello } from "../src/index.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const root = `${here}../../../../`;
const binary = `${root}target/debug/clankermux`;
const built = existsSync(binary);

/** Frames over a stream socket, one `onMessage` per frame. */
class StreamTransport implements Transport {
  private buffer = new Uint8Array(0);
  onMessage: ((bytes: Uint8Array) => void) | undefined;
  onClose: (() => void) | undefined;

  constructor(private readonly socket: Socket) {
    socket.on("data", (chunk: Buffer) => this.feed(chunk));
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

  private feed(chunk: Uint8Array): void {
    const joined = new Uint8Array(this.buffer.length + chunk.length);
    joined.set(this.buffer);
    joined.set(chunk, this.buffer.length);
    let pos = 0;
    while (joined.length - pos >= HEADER_LEN) {
      const header = parseHeader(joined.subarray(pos), 1 << 30);
      const end = pos + HEADER_LEN + header.length;
      if (end > joined.length) break;
      this.onMessage?.(joined.slice(pos, end));
      pos = end;
    }
    this.buffer = joined.slice(pos);
  }
}

let dir: string;
let socket: string;
let server: ReturnType<typeof spawn>;
let engine: Engine;

beforeAll(async () => {
  if (!built) return;
  engine = await Engine.load(readFileSync(`${here}../../engine/ghostty-vt.wasm`));
  dir = mkdtempSync(join(tmpdir(), "cmx-"));
  socket = join(dir, "sock");
  server = spawn(binary, ["--socket", socket, "serve"], { stdio: "ignore" });
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

afterAll(() => {
  server?.kill();
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
  await conn.request("write", { session: session.id, data: btoa("one\n") });
  for (;;) {
    const read = await conn.call<Read>("read", { session: session.id });
    if (read.text.includes("one\none")) break;
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
  await again.request("subscribe", {});
  again.sendJson(FrameType.Viewport, 1, { cols: 30, rows: 6 });
  actions = await drive(a, queue, (all) => all.some((x) => x.type === "resized"));
  expect(actions).toContainEqual({ type: "resized", cols: 30, rows: 6 });
  expect(a.term()!.cols).toBe(30);
  await again.request("signal", { session: session.id, signal: "KILL" });
  actions = await drive(a, queue, () => a.ended());
  expect(actions.at(-1)).toEqual({ type: "exited", exit: { signal: "KILL" } });
  for (let i = 0; i < 100 && !events.some((e) => e.event === "session_exited"); i++) {
    await new Promise((r) => setTimeout(r, 10));
  }
  expect(events.some((e) => e.event === "session_exited" && e.session.id === session.id)).toBe(true);

  // Server errors arrive typed; a second HELLO ends the connection.
  await expect(again.request("inspect", { session: "nope" })).rejects.toMatchObject({
    server: { code: "no_such_session" },
  });
  const ended = new Promise((resolve) => (again.onEnd = resolve));
  again.sendJson(FrameType.Hello, 0, hello("test"));
  await expect(ended).resolves.toMatchObject({ kind: "server", server: { code: "protocol_error" } });
  a.free();
});
