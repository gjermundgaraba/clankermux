import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { beforeAll, expect, test } from "vitest";

import { Engine, readyEnd } from "../../src/engine/index.ts";
import { type Attached, type ErrorMessage, type Frame, FrameType } from "../../src/proto/index.ts";

import { Attachment } from "../../src/client/attachment.ts";
import { output, session, welcome } from "./bodies.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
let engine: Engine;
beforeAll(async () => {
  engine = await Engine.load(readFileSync(`${here}../../ghostty-vt.wasm`));
});

function frame(type: FrameType, payload: Uint8Array): Frame {
  return { header: { type, channel: 1, length: payload.length }, payload };
}

function json(type: FrameType, body: unknown): Frame {
  return frame(type, new TextEncoder().encode(JSON.stringify(body)));
}

const outputFrame = (offset: number, text: string) => frame(FrameType.Output, output(offset, text));

function attached(offset: number, marker: number, bytes: Uint8Array): Frame {
  const body: Attached = {
    mode: "snapshot",
    offset,
    marker,
    cols: 10,
    rows: 3,
    session: session("s"),
    snapshot_bytes: bytes.length,
  };
  return json(FrameType.Attached, body);
}

function snapshotOf(text: string): Uint8Array {
  const source = engine.terminalNew(10, 3);
  source.write(new TextEncoder().encode(text));
  const bytes = source.snapshot();
  source.free();
  return bytes;
}

test("bootstraps, holds after a resize, and resumes", () => {
  const bytes = snapshotOf("one\r\ntwo\r\n");
  const ready = readyEnd(bytes)!;
  const a = new Attachment(engine);
  a.openOn(welcome("inc"));
  expect(a.handle(attached(100, 2, bytes))).toEqual([]);
  expect(a.handle(frame(FrameType.Snapshot, bytes.subarray(0, ready - 1)))).toEqual([]);
  expect(a.handle(frame(FrameType.Snapshot, bytes.subarray(ready - 1, ready)))).toEqual([{ type: "screen" }]);
  expect(a.handle(frame(FrameType.SnapshotReady, new Uint8Array()))).toEqual([]);
  expect(a.term()!.formatPlain().trimEnd()).toBe("one\ntwo");
  expect(a.handle(outputFrame(100, "x"))).toEqual([{ type: "output", bytes: new TextEncoder().encode("x") }]);
  expect(a.handle(json(FrameType.Resized, { seq: 3, offset: 101, cols: 20, rows: 5 }))).toEqual([]);
  expect(a.handle(outputFrame(101, "y"))).toEqual([]);
  expect(a.handle(frame(FrameType.Snapshot, bytes.subarray(ready)))).toEqual([]);
  expect(a.handle(frame(FrameType.SnapshotEnd, new Uint8Array()))).toEqual([
    { type: "resized", cols: 20, rows: 5 },
    { type: "output", bytes: new TextEncoder().encode("y") },
  ]);
  expect(a.term()!.formatPlain().trimEnd()).toBe("one\ntwo\nxy");
  expect(a.term()!.cols).toBe(20);
  expect(a.resume()).toEqual({ incarnation: "inc", offset: 102, marker: 3 });
  expect(() => a.handle(outputFrame(5, "z"))).toThrow(expect.objectContaining({ kind: "desync", message: expect.stringMatching(/OUTPUT at 5/) }));
  expect(() => a.handle(json(FrameType.Resized, { seq: 9, offset: 102, cols: 1, rows: 1 }))).toThrow(
    expect.objectContaining({ kind: "desync" }),
  );
  expect(a.handle(json(FrameType.Exited, { seq: 4, offset: 102, exit: { code: 0 } }))).toEqual([
    { type: "exited", exit: { code: 0 } },
  ]);
  expect(a.ended()).toBe(true);
  expect(a.closed()).toBe(false);
  expect(a.resume()).toBeUndefined();
  // The terminal stays for display but takes no more of the stream.
  expect(a.term()!.formatPlain().trimEnd()).toBe("one\ntwo\nxy");
  expect(() => a.handle(outputFrame(102, "late"))).toThrow(/outside the stream/);
  expect(a.term()!.formatPlain().trimEnd()).toBe("one\ntwo\nxy");
  a.free();
});

test("an EXITED during a bootstrap waits for SNAPSHOT_END", () => {
  const bytes = snapshotOf("one\r\ntwo\r\n");
  const ready = readyEnd(bytes)!;
  const a = new Attachment(engine);
  a.openOn(welcome("inc"));
  a.handle(attached(100, 0, bytes));
  expect(a.handle(frame(FrameType.Snapshot, bytes.subarray(0, ready)))).toEqual([{ type: "screen" }]);
  a.handle(frame(FrameType.SnapshotReady, new Uint8Array()));
  expect(a.handle(json(FrameType.Exited, { seq: 1, offset: 100, exit: { code: 3 } }))).toEqual([]);
  expect(a.ended()).toBe(false);
  a.handle(frame(FrameType.Snapshot, bytes.subarray(ready)));
  expect(a.handle(frame(FrameType.SnapshotEnd, new Uint8Array()))).toEqual([{ type: "exited", exit: { code: 3 } }]);
  expect(a.ended()).toBe(true);
  expect(() => a.handle(frame(FrameType.SnapshotEnd, new Uint8Array()))).toThrow(/outside a bootstrap/);
  expect(a.ended()).toBe(true);
  a.free();
});

test("a snapshot cut before FINISH is a protocol error", () => {
  const bytes = snapshotOf("one\r\ntwo\r\n");
  const ready = readyEnd(bytes)!;
  const a = new Attachment(engine);
  a.openOn(welcome("inc"));
  a.handle(attached(0, 0, bytes));
  a.handle(frame(FrameType.Snapshot, bytes.subarray(0, ready)));
  a.handle(frame(FrameType.SnapshotReady, new Uint8Array()));
  a.handle(frame(FrameType.Snapshot, bytes.subarray(ready, bytes.length - 1)));
  expect(() => a.handle(frame(FrameType.SnapshotEnd, new Uint8Array()))).toThrow(
    expect.objectContaining({ kind: "protocol", message: expect.stringMatching(/FINISH/) }),
  );
  a.free();
});

test("the cursor keeps the incarnation it was obtained under", () => {
  const bytes = snapshotOf("");
  const a = new Attachment(engine);
  const bootstrap = () => {
    a.handle(attached(7, 0, bytes));
    a.handle(frame(FrameType.Snapshot, bytes));
    a.handle(frame(FrameType.SnapshotReady, new Uint8Array()));
    a.handle(frame(FrameType.SnapshotEnd, new Uint8Array()));
  };
  a.openOn(welcome("first"));
  bootstrap();
  expect(a.resume()!.incarnation).toBe("first");
  a.openOn(welcome("second"));
  expect(a.resume()!.incarnation).toBe("first");
  bootstrap();
  expect(a.resume()!.incarnation).toBe("second");
  a.free();
});

test("a gap discards and errors classify", () => {
  const a = new Attachment(engine);
  expect(a.handle(json(FrameType.Gap, { from: 1, to: 9 }))).toEqual([{ type: "gap" }]);
  expect(a.ended()).toBe(false);
  const soft: ErrorMessage = { code: "read_only", message: "" };
  expect(a.handle(json(FrameType.Error, soft))).toEqual([{ type: "error", error: soft }]);
  expect(a.ended()).toBe(false);
  const hard: ErrorMessage = { code: "too_slow", message: "" };
  a.handle(json(FrameType.Error, hard));
  expect(a.ended()).toBe(true);
  expect(a.closed()).toBe(true);
  expect(() => a.handle(json(FrameType.Welcome, {}))).toThrow(/on an attachment/);
});
