import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";

import { FrameDecoder, FrameType, HeaderError, encodeFrame, encodeOutput, parseOutput } from "../src/index.ts";
import type { Limits } from "../src/index.ts";

const vectors = fileURLToPath(new URL("../../../../crates/proto/vectors/", import.meta.url));
const limits: Limits = JSON.parse(readFileSync(`${vectors}limits.json`, "utf8"));

function hex(s: string): Uint8Array {
  return Uint8Array.from(Buffer.from(s, "hex"));
}

type Vector =
  | { name: string; hex: string; type: number; channel: number; payload: string }
  | { name: string; hex: string; error: string };

test("frame vectors", () => {
  const frames: Vector[] = JSON.parse(readFileSync(`${vectors}frames.json`, "utf8"));
  for (const v of frames) {
    const decoder = new FrameDecoder();
    decoder.push(hex(v.hex));
    if ("error" in v) {
      expect(() => decoder.next(limits.max_frame), v.name).toThrow(new HeaderError(v.error));
      continue;
    }
    const frame = decoder.next(limits.max_frame);
    expect(frame, v.name).toBeDefined();
    expect(frame!.header, v.name).toEqual({ type: v.type, channel: v.channel, length: v.payload.length / 2 });
    expect(Buffer.from(frame!.payload).toString("hex"), v.name).toBe(v.payload);
    expect(decoder.next(limits.max_frame), `${v.name}: trailing bytes`).toBeUndefined();
    const expected = hex(v.hex);
    expected[1] = 0;
    expect(encodeFrame(frame!.header.type, v.channel, frame!.payload), `${v.name}: re-encode`).toEqual(expected);
  }
});

test("frames arrive in pieces", () => {
  const bytes = encodeFrame(FrameType.Output, 7, encodeOutput(42n, new TextEncoder().encode("abc")));
  const decoder = new FrameDecoder();
  for (let i = 0; i < bytes.length - 1; i++) {
    decoder.push(bytes.subarray(i, i + 1));
    expect(decoder.next(1024)).toBeUndefined();
  }
  decoder.push(bytes.subarray(bytes.length - 1));
  const frame = decoder.next(1024)!;
  expect(frame.header).toEqual({ type: FrameType.Output, channel: 7, length: 11 });
  const output = parseOutput(frame.payload)!;
  expect(output.offset).toBe(42n);
  expect(new TextDecoder().decode(output.bytes)).toBe("abc");
  expect(parseOutput(frame.payload.subarray(0, 7))).toBeUndefined();
});

test("message vectors parse as JSON objects", () => {
  const messages: { [name: string]: unknown } = JSON.parse(readFileSync(`${vectors}messages.json`, "utf8"));
  expect(Object.keys(messages).length).toBeGreaterThan(20);
  for (const value of Object.values(messages)) expect(typeof value).toBe("object");
});
