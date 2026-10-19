import { HEADER_LEN, parseHeader } from "./frame.ts";

/**
 * Splits an ordered byte stream into frames, for carriers that are not
 * message-oriented (a Unix or TCP socket). One call to `feed` per chunk;
 * each complete frame comes back as its own bytes, header included.
 */
export class StreamFramer {
  private buffer = new Uint8Array(0);
  private readonly maxFrame: number;

  constructor(maxFrame: number) {
    this.maxFrame = maxFrame;
  }

  feed(chunk: Uint8Array): Uint8Array[] {
    const joined = new Uint8Array(this.buffer.length + chunk.length);
    joined.set(this.buffer);
    joined.set(chunk, this.buffer.length);
    const frames: Uint8Array[] = [];
    let pos = 0;
    while (joined.length - pos >= HEADER_LEN) {
      const header = parseHeader(joined.subarray(pos), this.maxFrame);
      const end = pos + HEADER_LEN + header.length;
      if (end > joined.length) break;
      frames.push(joined.slice(pos, end));
      pos = end;
    }
    this.buffer = joined.slice(pos);
    return frames;
  }
}
