import { type Engine, Terminal } from "./index.ts";

const MAGIC_LEN = 8;
const VERSION_LEN = 2;
const ENVELOPE_LEN = MAGIC_LEN + VERSION_LEN;

const TAG_LEN = 2;
const PAYLOAD_LENGTH_LEN = 4;
const CRC32C_LEN = 4;
const RECORD_HEADER_LEN = TAG_LEN + PAYLOAD_LENGTH_LEN + CRC32C_LEN;

const TAG_PAGE = 3;
const TAG_READY = 5;
const TAG_FINISH = 6;

function recordAt(buf: Uint8Array, len: number, pos: number): [number, number] | undefined {
  if (pos + RECORD_HEADER_LEN > len) return undefined;
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const tag = view.getUint16(pos, true);
  const payload = view.getUint32(pos + TAG_LEN, true);
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

export function readyEnd(buf: Uint8Array, len = buf.length): number | undefined {
  return endOfStep(buf, len, ENVELOPE_LEN, (tag) => tag === TAG_READY);
}

export function stepEnd(buf: Uint8Array, pos: number, len = buf.length): number | undefined {
  return endOfStep(buf, len, pos, (tag) => tag === TAG_PAGE || tag === TAG_FINISH);
}

export class Decoder {
  private readonly buffer: number;
  private decoder: number;
  private filled = 0;
  private consumed = 0;
  private term: Terminal | undefined;
  private complete = false;

  constructor(
    readonly engine: Engine,
    readonly capacity: number,
  ) {
    this.buffer = engine.alloc(capacity);
    engine.memory.fill(0, this.buffer, this.buffer + capacity);
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

  readyAvailable(): boolean {
    return readyEnd(this.source, this.filled) !== undefined;
  }

  stepAvailable(): boolean {
    return !this.complete && stepEnd(this.source, this.consumed, this.filled) !== undefined;
  }

  ready(): Terminal {
    const end = readyEnd(this.source, this.filled);
    if (end === undefined) throw new Error("READY not buffered");
    this.term = new Terminal(this.engine, this.engine.take("ghostty_snapshot_decoder_ready", this.decoder));
    this.consumed = end;
    return this.term;
  }

  nextPage(): boolean {
    const end = stepEnd(this.source, this.consumed, this.filled);
    if (end === undefined) throw new Error("next page not buffered");
    const page = this.engine.present("ghostty_snapshot_decoder_next", this.decoder);
    this.consumed = end;
    if (!page) this.complete = true;
    return page;
  }

  terminal(): Terminal | undefined {
    return this.term;
  }

  finished(): boolean {
    return this.complete;
  }

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
