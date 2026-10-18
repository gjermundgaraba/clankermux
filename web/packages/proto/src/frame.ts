export const HEADER_LEN = 10;

export const FrameType = {
  Hello: 0x01,
  Request: 0x02,
  Open: 0x10,
  Input: 0x11,
  Viewport: 0x12,
  Close: 0x13,
  Welcome: 0x81,
  Reply: 0x82,
  Event: 0x83,
  Error: 0x8f,
  Attached: 0x90,
  Snapshot: 0x91,
  SnapshotReady: 0x92,
  SnapshotEnd: 0x93,
  Output: 0x94,
  Resized: 0x95,
  Exited: 0x96,
  Gap: 0x97,
  Closed: 0x99,
} as const;

export type FrameType = (typeof FrameType)[keyof typeof FrameType];

const KNOWN_TYPES = new Set<number>(Object.values(FrameType));

export function isFrameType(byte: number): byte is FrameType {
  return KNOWN_TYPES.has(byte);
}

export interface Header {
  type: FrameType;
  channel: number;
  length: number;
}

export type FrameErrorKind = "unknown_type" | "too_long" | "short" | "length_mismatch";

export class FrameError extends Error {
  readonly kind: FrameErrorKind;

  // Erasable syntax only: node runs this package's sources with type
  // stripping (the headless driver).
  constructor(kind: FrameErrorKind, message: string) {
    super(message);
    this.kind = kind;
  }
}

export function encodeHeader(type: FrameType, channel: number, length: number): Uint8Array {
  const out = new Uint8Array(HEADER_LEN);
  const view = new DataView(out.buffer);
  out[0] = type;
  view.setUint32(2, channel, true);
  view.setUint32(6, length, true);
  return out;
}

/** Parses a header, ignoring `flags`. */
export function parseHeader(bytes: Uint8Array, maxFrame: number): Header {
  if (bytes.length < HEADER_LEN) throw new FrameError("short", "message shorter than a frame header");
  const type = bytes[0]!;
  if (!isFrameType(type)) {
    throw new FrameError("unknown_type", `unknown frame type 0x${type.toString(16).padStart(2, "0")}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, HEADER_LEN);
  const channel = view.getUint32(2, true);
  const length = view.getUint32(6, true);
  if (length > maxFrame) throw new FrameError("too_long", `frame length ${length} exceeds max_frame ${maxFrame}`);
  return { type, channel, length };
}

export function encodeFrame(type: FrameType, channel: number, payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(HEADER_LEN + payload.length);
  out.set(encodeHeader(type, channel, payload.length));
  out.set(payload, HEADER_LEN);
  return out;
}

export interface Frame {
  header: Header;
  payload: Uint8Array;
}

/** Parses one message that carries exactly one frame. */
export function parseFrame(message: Uint8Array, maxFrame: number): Frame {
  const header = parseHeader(message, maxFrame);
  const payload = message.subarray(HEADER_LEN);
  if (header.length !== payload.length) {
    throw new FrameError("length_mismatch", `frame length ${header.length} with ${payload.length} payload bytes`);
  }
  return { header, payload };
}

/** `OUTPUT` payload: the stream offset of the first byte (u64 LE), then the bytes. */
export function parseOutput(payload: Uint8Array): { offset: number; bytes: Uint8Array } | undefined {
  if (payload.length < 8) return undefined;
  const view = new DataView(payload.buffer, payload.byteOffset, 8);
  const offset = view.getUint32(4, true) * 2 ** 32 + view.getUint32(0, true);
  if (!Number.isSafeInteger(offset)) return undefined;
  return { offset, bytes: payload.subarray(8) };
}
