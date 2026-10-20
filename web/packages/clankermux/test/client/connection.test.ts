import { expect, test } from "vitest";

import limits from "../../../../../crates/proto/vectors/limits.json";
import { type Frame, FrameType, encodeFrame, parseFrame } from "../../src/proto/index.ts";

import { ClientError, Connection, type Transport, hello } from "../../src/client/connection.ts";

/** A transport whose peer is the test. */
class Fake implements Transport {
  sent: Frame[] = [];
  closed = false;
  onMessage: ((bytes: Uint8Array) => void) | undefined;
  onClose: (() => void) | undefined;

  send(bytes: Uint8Array): void {
    this.sent.push(parseFrame(bytes, 1 << 20));
  }

  close(): void {
    this.closed = true;
    this.onClose?.();
  }

  /** The server sends a JSON frame. */
  push(type: FrameType, channel: number, body: unknown): void {
    this.onMessage?.(encodeFrame(type, channel, new TextEncoder().encode(JSON.stringify(body))));
  }

  json(frame: Frame): unknown {
    return JSON.parse(new TextDecoder().decode(frame.payload));
  }
}

const welcome = {
  version: 1,
  server: { name: "test", version: "0" },
  engine: "x",
  incarnation: "i",
  capabilities: [],
  limits: { max_frame: 1 << 20 },
};

async function connect(): Promise<[Fake, Connection]> {
  const fake = new Fake();
  const connecting = Connection.connect(fake, hello("test"));
  expect(fake.sent[0]!.header.type).toBe(FrameType.Hello);
  fake.push(FrameType.Welcome, 0, welcome);
  return [fake, await connecting];
}

test("requests are answered by id and errors typed", async () => {
  const [fake, conn] = await connect();
  const first = conn.call<{ ok: boolean }>("list", {});
  const second = conn.call("inspect", { session: "x" });
  const ids = fake.sent.slice(1).map((f) => (fake.json(f) as { id: number }).id);
  expect(ids).toEqual([1, 2]);
  fake.push(FrameType.Reply, 0, { id: 2, result: null });
  fake.push(FrameType.Error, 0, { id: 1, code: "no_such_session", message: "nope" });
  // A reply for a request no longer waited for is dropped.
  fake.push(FrameType.Reply, 0, { id: 99, result: 1 });
  await expect(second).resolves.toBeNull();
  await expect(first).rejects.toMatchObject({ kind: "server", server: { code: "no_such_session" } });
});

test("an error without id ends the connection; open and requests refuse afterwards", async () => {
  const [fake, conn] = await connect();
  const ended: ClientError[] = [];
  conn.onEnd = (e) => ended.push(e);
  const pending = conn.call("list", {});
  const frames: Frame[] = [];
  conn.open(1, { session: "s" }, (f) => frames.push(f));
  fake.push(FrameType.Error, 0, { code: "protocol_error", message: "bad" });
  await expect(pending).rejects.toMatchObject({ kind: "server" });
  expect(ended.map((e) => e.kind)).toEqual(["server"]);
  expect(fake.closed).toBe(true);
  expect(() => conn.open(1, { session: "s" }, () => {})).toThrow(ClientError);
  await expect(conn.call("list", {})).rejects.toMatchObject({ kind: "server" });
  // Channel receivers are gone with the connection.
  fake.push(FrameType.Output, 1, {});
  expect(frames).toEqual([]);
  // A later transport close reports nothing more.
  fake.close();
  expect(ended).toHaveLength(1);
});

test("channel frames and events are routed", async () => {
  const [fake, conn] = await connect();
  const frames: Frame[] = [];
  conn.open(1, { session: "s" }, (f) => frames.push(f));
  expect(fake.json(fake.sent.at(-1)!)).toEqual({ session: "s" });
  const events: unknown[] = [];
  conn.onEvent = (e) => events.push(e);
  fake.push(FrameType.Attached, 1, { mode: "resume" });
  fake.push(FrameType.Event, 0, { event: "session_removed", id: "s" });
  expect(frames.map((f) => f.header.type)).toEqual([FrameType.Attached]);
  expect(events).toEqual([{ event: "session_removed", id: "s" }]);
  conn.release(1);
  fake.push(FrameType.Closed, 1, { reason: "closed" });
  expect(frames).toHaveLength(1);
  // A transport close ends with `closed`.
  const ended: ClientError[] = [];
  conn.onEnd = (e) => ended.push(e);
  fake.close();
  expect(ended.map((e) => e.kind)).toEqual(["closed"]);
});

test("a rejected or closed handshake rejects", async () => {
  const fake = new Fake();
  const connecting = Connection.connect(fake, hello("test", "other"));
  fake.push(FrameType.Error, 0, { code: "engine_mismatch", message: "no" });
  await expect(connecting).rejects.toMatchObject({ server: { code: "engine_mismatch" } });
  const closing = new Fake();
  const second = Connection.connect(closing, hello("test"));
  closing.close();
  await expect(second).rejects.toMatchObject({ kind: "closed" });
});

test("before WELCOME a frame may be as long as the protocol's default max_frame", async () => {
  const body = new TextEncoder().encode(JSON.stringify(welcome));
  const handshake = (length: number) => {
    const fake = new Fake();
    const connecting = Connection.connect(fake, hello("test"));
    // Trailing spaces, which JSON allows, make up the length.
    const padded = new Uint8Array(length).fill(0x20);
    padded.set(body);
    fake.onMessage?.(encodeFrame(FrameType.Welcome, 0, padded));
    return connecting;
  };
  await expect(handshake(limits.max_frame)).resolves.toBeInstanceOf(Connection);
  await expect(handshake(limits.max_frame + 1)).rejects.toMatchObject({ kind: "protocol", message: /exceeds max_frame/ });
});
