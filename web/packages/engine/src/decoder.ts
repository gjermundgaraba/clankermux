import { type Engine, EngineError, RESULT, Terminal } from "./index.ts";

/** Bytes before the first record: the 8-byte magic and the u16 version. */
export const ENVELOPE_LEN = 10;
/** Bytes in a record header: tag u16, payload length u32, CRC32C u32. */
export const RECORD_HEADER_LEN = 10;

const TAG_PAGE = 3;
const TAG_READY = 5;
const TAG_FINISH = 6;

/** The record starting at `pos`: its tag and the position just past it. */
function recordAt(buf: Uint8Array, len: number, pos: number): [number, number] | undefined {
  if (pos + RECORD_HEADER_LEN > len) return undefined;
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const tag = view.getUint16(pos, true);
  const payload = view.getUint32(pos + 2, true);
  const end = pos + RECORD_HEADER_LEN + payload;
  return end <= len ? [tag, end] : undefined;
}

function endOfStep(buf: Uint8Array, len: number, pos: number, last: (tag: number) => boolean): number | undefined {
  for (;;) {
    const record = recordAt(buf, len, pos);
    if (!record) return undefined;
    if (last(record[0])) return record[1];
    pos = record[1];
  }
}

/** Position just past the `READY` record, once every byte through it is in `buf`. */
export function readyEnd(buf: Uint8Array, len = buf.length): number | undefined {
  return endOfStep(buf, len, ENVELOPE_LEN, (tag) => tag === TAG_READY);
}

/** Position just past the next `PAGE` or `FINISH` record at or after `pos`. */
export function stepEnd(buf: Uint8Array, pos: number, len = buf.length): number | undefined {
  return endOfStep(buf, len, pos, (tag) => tag === TAG_PAGE || tag === TAG_FINISH);
}

/**
 * An incremental snapshot decoder over a buffer of known capacity, filled
 * as bytes arrive. The library reads the buffer lazily, so `ready` and
 * `nextPage` are called only once the bytes they consume are in place.
 */
export class Decoder {
  private readonly buffer: number;
  private decoder: number;
  private filled = 0;
  private consumed = 0;
  private term: Terminal | undefined;
  private finished = false;

  constructor(
    readonly engine: Engine,
    readonly capacity: number,
  ) {
    this.buffer = engine.alloc(capacity);
    this.decoder = engine.construct("ghostty_snapshot_decoder_new_buf", this.buffer, capacity);
  }

  setMaxContinuationBytes(max: number): void {
    this.engine.scratch(4, (value) => {
      this.engine.view.setUint32(value, max, true);
      this.engine.check(
        "ghostty_snapshot_decoder_set",
        this.decoder,
        this.engine.enumValue("GhosttySnapshotDecoderOption", "MAX_CONTINUATION_BYTES"),
        value,
      );
    });
  }

  push(bytes: Uint8Array): void {
    if (this.filled + bytes.length > this.capacity) {
      throw new Error(`snapshot exceeds its declared ${this.capacity} bytes`);
    }
    this.engine.memory.set(bytes, this.buffer + this.filled);
    this.filled += bytes.length;
  }

  private get source(): Uint8Array {
    return this.engine.memory.subarray(this.buffer, this.buffer + this.capacity);
  }

  /** Whether every byte through `READY` is buffered. */
  readyAvailable(): boolean {
    return readyEnd(this.source, this.filled) !== undefined;
  }

  stepAvailable(): boolean {
    return !this.finished && stepEnd(this.source, this.consumed, this.filled) !== undefined;
  }

  /** Decodes through `READY`; the terminal is then renderable. */
  ready(): Terminal {
    const end = readyEnd(this.source, this.filled);
    if (end === undefined) throw new Error("READY not buffered");
    this.term = new Terminal(this.engine, this.engine.take("ghostty_snapshot_decoder_ready", this.decoder));
    this.consumed = end;
    return this.term;
  }

  /** Decodes one history page. False once `FINISH` is validated. */
  nextPage(): boolean {
    const end = stepEnd(this.source, this.consumed, this.filled);
    if (end === undefined) throw new Error("next page not buffered");
    const code = this.engine.call("ghostty_snapshot_decoder_next", this.decoder);
    this.consumed = end;
    if (code === RESULT.NO_VALUE) {
      this.finished = true;
      return false;
    }
    if (code !== RESULT.SUCCESS) throw new EngineError("ghostty_snapshot_decoder_next", code);
    return true;
  }

  terminal(): Terminal | undefined {
    return this.term;
  }

  /** Frees the decoder and its buffer; the terminal, finished or not, is the caller's. */
  release(): Terminal | undefined {
    const term = this.term;
    this.term = undefined;
    this.free();
    return term;
  }

  free(): void {
    if (this.decoder === 0) return;
    this.engine.call("ghostty_snapshot_decoder_free", this.decoder);
    this.engine.free(this.buffer, this.capacity);
    this.decoder = 0;
    this.term?.free();
    this.term = undefined;
  }
}
