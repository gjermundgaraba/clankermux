/**
 * Framing properties with a seeded generator: no input throws anything
 * but a FrameError, and every chunking of a valid stream yields the same
 * frames.
 */
import { expect, test } from "vitest";

import { FrameError, FrameType, HEADER_LEN, StreamFramer, encodeFrame, parseFrame } from "../src/index.ts";

const MAX_FRAME = 1024;
const CASES = Number(process.env["CLANKERMUX_FUZZ_CASES"] ?? 400);

/** xorshift32, so a failure reproduces from its case number. */
function rng(seed: number): () => number {
  let x = seed || 1;
  return () => {
    x ^= x << 13;
    x ^= x >>> 17;
    x ^= x << 5;
    return (x >>> 0) / 0x1_0000_0000;
  };
}

const int = (r: () => number, max: number) => Math.floor(r() * max);
const bytes = (r: () => number, len: number) => Uint8Array.from({ length: len }, () => int(r, 256));
const types = Object.values(FrameType);

/** `stream` split at random points, concatenation preserved. */
function chunk(r: () => number, stream: Uint8Array): Uint8Array[] {
  const cuts = Array.from({ length: int(r, 8) }, () => int(r, stream.length + 1)).sort((a, b) => a - b);
  const out: Uint8Array[] = [];
  let start = 0;
  for (const cut of [...cuts, stream.length]) {
    out.push(stream.subarray(start, cut));
    start = cut;
  }
  return out;
}

function decodeAll(chunks: Uint8Array[]): { frames: Uint8Array[]; error: FrameError | undefined } {
  const framer = new StreamFramer(MAX_FRAME);
  const frames: Uint8Array[] = [];
  for (const c of chunks) {
    try {
      frames.push(...framer.feed(c));
    } catch (err) {
      if (!(err instanceof FrameError)) throw err;
      return { frames, error: err };
    }
  }
  return { frames, error: undefined };
}

test("a valid stream decodes the same under every chunking", () => {
  for (let n = 0; n < CASES; n++) {
    const r = rng(n + 1);
    const frames = Array.from({ length: int(r, 12) }, () => ({
      type: types[int(r, types.length)]!,
      channel: int(r, 0x1_0000_0000),
      payload: bytes(r, int(r, 300)),
    }));
    const encoded = frames.map((f) => encodeFrame(f.type, f.channel, f.payload));
    const stream = new Uint8Array(encoded.reduce((n, f) => n + f.length, 0));
    let pos = 0;
    for (const f of encoded) {
      stream.set(f, pos);
      pos += f.length;
    }
    const { frames: decoded, error } = decodeAll(chunk(r, stream));
    expect(error, `case ${n}`).toBeUndefined();
    expect(decoded.length, `case ${n}`).toBe(frames.length);
    decoded.forEach((bytes, i) => {
      const frame = parseFrame(bytes, MAX_FRAME);
      expect(frame.header, `case ${n} frame ${i}`).toEqual({ type: frames[i]!.type, channel: frames[i]!.channel, length: frames[i]!.payload.length });
      expect(frame.payload, `case ${n} frame ${i}`).toEqual(frames[i]!.payload);
    });
  }
});

test("any bytes decode or fail with a FrameError", () => {
  for (let n = 0; n < CASES; n++) {
    const r = rng(1000 + n);
    const stream = bytes(r, int(r, 2048));
    const { frames, error } = decodeAll(chunk(r, stream));
    // Each frame is a whole message a message parser accepts.
    let pos = 0;
    for (const f of frames) {
      expect(f.length, `case ${n}`).toBe(HEADER_LEN + parseFrame(f, MAX_FRAME).header.length);
      expect(f, `case ${n}`).toEqual(stream.subarray(pos, pos + f.length));
      pos += f.length;
    }
    if (error) {
      // The failure is at the header after the last frame.
      expect(() => parseFrame(stream.subarray(pos), MAX_FRAME), `case ${n}`).toThrow(FrameError);
    }
    try {
      parseFrame(stream, MAX_FRAME);
    } catch (err) {
      expect(err, `case ${n}`).toBeInstanceOf(FrameError);
    }
  }
});
