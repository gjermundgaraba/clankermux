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

export class HeaderError extends Error {}

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
  const type = bytes[0]!;
  if (!isFrameType(type)) throw new HeaderError(`unknown frame type 0x${type.toString(16).padStart(2, "0")}`);
  const view = new DataView(bytes.buffer, bytes.byteOffset, HEADER_LEN);
  const channel = view.getUint32(2, true);
  const length = view.getUint32(6, true);
  if (length > maxFrame) throw new HeaderError(`frame length ${length} exceeds max_frame ${maxFrame}`);
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

/** Splits a byte stream into frames. */
export class FrameDecoder {
  private buf = new Uint8Array(0);
  private pos = 0;

  push(bytes: Uint8Array): void {
    const pending = this.buf.subarray(this.pos);
    const next = new Uint8Array(pending.length + bytes.length);
    next.set(pending);
    next.set(bytes, pending.length);
    this.buf = next;
    this.pos = 0;
  }

  /** The next complete frame, if buffered. A HeaderError poisons the stream. */
  next(maxFrame: number): Frame | undefined {
    if (this.buf.length - this.pos < HEADER_LEN) return undefined;
    const header = parseHeader(this.buf.subarray(this.pos, this.pos + HEADER_LEN), maxFrame);
    const end = this.pos + HEADER_LEN + header.length;
    if (end > this.buf.length) return undefined;
    const payload = this.buf.slice(this.pos + HEADER_LEN, end);
    this.pos = end;
    return { header, payload };
  }
}

/** `OUTPUT` payload: the stream offset of the first byte, then the bytes. */
export function encodeOutput(offset: bigint, bytes: Uint8Array): Uint8Array {
  const out = new Uint8Array(8 + bytes.length);
  new DataView(out.buffer).setBigUint64(0, offset, true);
  out.set(bytes, 8);
  return out;
}

export function parseOutput(payload: Uint8Array): { offset: bigint; bytes: Uint8Array } | undefined {
  if (payload.length < 8) return undefined;
  const offset = new DataView(payload.buffer, payload.byteOffset, 8).getBigUint64(0, true);
  return { offset, bytes: payload.subarray(8) };
}
