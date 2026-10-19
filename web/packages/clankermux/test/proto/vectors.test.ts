import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";

import { FrameError, FrameType, StreamFramer, encodeFrame, parseFrame, parseOutput } from "../../src/proto/index.ts";
import type { Limits } from "../../src/proto/index.ts";
import { vectors as messages } from "./messages.ts";

const dir = fileURLToPath(new URL("../../../../../crates/proto/vectors/", import.meta.url));
const limits: Limits = JSON.parse(readFileSync(`${dir}limits.json`, "utf8"));

function hex(s: string): Uint8Array {
  return Uint8Array.from(Buffer.from(s, "hex"));
}

interface FrameVectors {
  types: Record<string, number>;
  frames: (
    | { name: string; hex: string; type: number; channel: number; payload: string }
    | { name: string; hex: string; error: string }
  )[];
}

const frames: FrameVectors = JSON.parse(readFileSync(`${dir}frames.json`, "utf8"));

function snake(name: string): string {
  return name.replace(/[A-Z]/g, (c, i) => (i === 0 ? "" : "_") + c.toLowerCase());
}

test("frame types match the vector table", () => {
  const table = Object.fromEntries(Object.entries(FrameType).map(([name, value]) => [snake(name), value]));
  expect(table).toEqual(frames.types);
});

test("frame vectors", () => {
  for (const v of frames.frames) {
    if ("error" in v) {
      let error: unknown;
      try {
        parseFrame(hex(v.hex), limits.max_frame);
      } catch (e) {
        error = e;
      }
      expect(error, v.name).toBeInstanceOf(FrameError);
      expect((error as FrameError).kind, v.name).toBe(v.error);
      continue;
    }
    const frame = parseFrame(hex(v.hex), limits.max_frame);
    expect(frame.header, v.name).toEqual({ type: v.type, channel: v.channel, length: v.payload.length / 2 });
    expect(Buffer.from(frame.payload).toString("hex"), v.name).toBe(v.payload);
    const expected = hex(v.hex);
    expected[1] = 0;
    expect(encodeFrame(frame.header.type, v.channel, frame.payload), `${v.name}: re-encode`).toEqual(expected);
  }
});

test("output offsets above 2^32 and payloads shorter than one", () => {
  const output = parseOutput(hex("2a00000000000000616263"))!;
  expect(output.offset).toBe(42);
  expect(new TextDecoder().decode(output.bytes)).toBe("abc");
  expect(parseOutput(hex("0000000001000000"))!.offset).toBe(2 ** 32);
  expect(parseOutput(hex("0000000000002000"))).toBeUndefined();
  expect(parseOutput(hex("2a000000000000"))).toBeUndefined();
});

test("message vectors equal the typed literals", () => {
  const file = JSON.parse(readFileSync(`${dir}messages.json`, "utf8"));
  expect(file).toEqual(messages);
});

test("a stream is split into frames at any chunk boundary", () => {
  const a = encodeFrame(FrameType.Input, 1, new TextEncoder().encode("abc"));
  const b = encodeFrame(FrameType.Close, 1, new Uint8Array());
  const all = new Uint8Array([...a, ...b]);
  for (const cut of [0, 1, 9, 10, 12, 13, 20, all.length]) {
    const framer = new StreamFramer(1 << 20);
    const frames = [...framer.feed(all.subarray(0, cut)), ...framer.feed(all.subarray(cut))];
    expect(frames.map((f) => [...f])).toEqual([[...a], [...b]]);
  }
  expect(() => new StreamFramer(2).feed(a)).toThrow(FrameError);
});
