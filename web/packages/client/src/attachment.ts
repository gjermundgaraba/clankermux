import { Decoder, type Engine, type Terminal } from "@clankermux/engine";
import {
  type Attached,
  type Closed,
  type ErrorMessage,
  type Exit,
  type Exited,
  type Frame,
  FrameType,
  type Gap,
  type Resized,
  type Resume,
  type Session,
  type Welcome,
  parseOutput,
} from "@clankermux/proto";

import { ClientError } from "./connection.ts";

/** What a client does in response to frames. */
export type Action =
  /** The screen is renderable from the local engine. The session may
   * already have exited: see `Attachment.session`. */
  | { type: "screen" }
  | { type: "output"; bytes: Uint8Array }
  | { type: "resized"; cols: number; rows: number }
  | { type: "exited"; exit: Exit }
  /** Bytes were lost; the terminal was discarded and a new bootstrap follows. */
  | { type: "gap"; gap: Gap }
  | { type: "closed"; reason: Closed["reason"] }
  /** `read_only`, `not_running` and `input_overflow` keep the attachment;
   * any other code ended it. */
  | { type: "error"; error: ErrorMessage };

type State =
  | { kind: "opening" }
  /** `held`: stream frames after a `RESIZED` or `EXITED`, until `SNAPSHOT_END`. */
  | { kind: "bootstrapping"; decoder: Decoder; held: Frame[] }
  | { kind: "streaming"; term: Terminal }
  /** After the final marker, `CLOSED` or a fatal error. The terminal stays
   * for display only; `closed` says the server ended the attachment, so no
   * `CLOSE` is owed. */
  | { kind: "ended"; term: Terminal | undefined; closed: boolean };

const KEEPS_ATTACHMENT = new Set<ErrorMessage["code"]>(["read_only", "not_running", "input_overflow"]);

/**
 * The attachment state machine: frames in, actions out. It keeps the
 * local engine current and tracks the cursor for resuming.
 */
export class Attachment {
  private state: State = { kind: "opening" };
  private cursor = { offset: 0, marker: 0 };
  /** The server the cursor was obtained from, and its limit. */
  private incarnation = "";
  private continuation = 0;
  /** The server of the connection the next `ATTACHED` arrives on. */
  private next: { incarnation: string; continuation: number } | undefined;
  session: Session | undefined;

  constructor(readonly engine: Engine) {}

  /** Records the server an `OPEN` is about to go to; its incarnation
   * becomes the cursor's once that `ATTACHED` arrives. */
  openOn(welcome: Welcome): void {
    this.next = { incarnation: welcome.incarnation, continuation: welcome.limits.continuation };
  }

  /** The cursor to resume from, once the terminal reflects the stream. */
  resume(): Resume | undefined {
    return this.state.kind === "streaming"
      ? { incarnation: this.incarnation, offset: this.cursor.offset, marker: this.cursor.marker }
      : undefined;
  }

  ended(): boolean {
    return this.state.kind === "ended";
  }

  /** The server ended the attachment (`CLOSED` or a fatal `ERROR`). */
  closed(): boolean {
    return this.state.kind === "ended" && this.state.closed;
  }

  /** The local engine, once `READY` has been decoded. */
  term(): Terminal | undefined {
    switch (this.state.kind) {
      case "bootstrapping":
        return this.state.decoder.terminal();
      case "streaming":
      case "ended":
        return this.state.term;
      default:
        return undefined;
    }
  }

  handle(frame: Frame): Action[] {
    const actions: Action[] = [];
    this.step(frame, actions);
    return actions;
  }

  /** Releases the engine objects. */
  free(): void {
    this.discard();
    this.state = { kind: "ended", term: undefined, closed: true };
  }

  private json<T>(frame: Frame): T {
    try {
      return JSON.parse(new TextDecoder().decode(frame.payload)) as T;
    } catch (err) {
      throw ClientError.protocol(`malformed ${frame.header.type}: ${err}`);
    }
  }

  private step(frame: Frame, actions: Action[]): void {
    const type = frame.header.type;
    switch (type) {
      case FrameType.Attached: {
        const attached = this.json<Attached>(frame);
        if (this.next) {
          this.incarnation = this.next.incarnation;
          this.continuation = this.next.continuation;
          this.next = undefined;
        }
        this.cursor = { offset: attached.offset, marker: attached.marker };
        this.session = attached.session;
        if (attached.mode === "snapshot") {
          if (attached.snapshot_bytes === undefined) throw ClientError.protocol("ATTACHED without snapshot_bytes");
          this.discard();
          const decoder = new Decoder(this.engine, attached.snapshot_bytes);
          try {
            decoder.setMaxContinuationBytes(this.continuation);
          } catch (err) {
            decoder.free();
            throw err;
          }
          this.state = { kind: "bootstrapping", decoder, held: [] };
        } else if (this.state.kind !== "streaming") {
          throw ClientError.protocol("resume without a terminal");
        }
        break;
      }
      case FrameType.Snapshot: {
        if (this.state.kind !== "bootstrapping") throw ClientError.protocol("SNAPSHOT outside a bootstrap");
        const { decoder } = this.state;
        decoder.push(frame.payload);
        if (!decoder.terminal()) {
          if (decoder.readyAvailable()) {
            decoder.ready();
            actions.push({ type: "screen" });
          }
        } else {
          while (decoder.stepAvailable() && decoder.nextPage()) {}
        }
        break;
      }
      case FrameType.SnapshotReady:
        if (this.state.kind !== "bootstrapping") throw ClientError.protocol("SNAPSHOT_READY outside a bootstrap");
        if (!this.state.decoder.terminal()) throw ClientError.protocol("SNAPSHOT_READY before READY");
        break;
      case FrameType.SnapshotEnd: {
        if (this.state.kind !== "bootstrapping") throw ClientError.protocol("SNAPSHOT_END outside a bootstrap");
        const { decoder, held } = this.state;
        while (decoder.stepAvailable() && decoder.nextPage()) {}
        const term = decoder.release();
        if (!term) throw ClientError.protocol("SNAPSHOT_END before READY");
        this.state = { kind: "streaming", term };
        for (const f of held) this.step(f, actions);
        break;
      }
      case FrameType.Output:
      case FrameType.Resized:
      case FrameType.Exited:
        if (this.state.kind === "bootstrapping" && (this.state.held.length > 0 || type !== FrameType.Output)) {
          this.state.held.push(frame);
          return;
        }
        if (this.state.kind === "ended") throw ClientError.protocol(`${type} after the attachment ended`);
        this.stream(frame, actions);
        break;
      case FrameType.Gap: {
        const gap = this.json<Gap>(frame);
        this.discard();
        actions.push({ type: "gap", gap });
        break;
      }
      case FrameType.Closed: {
        const closed = this.json<Closed>(frame);
        this.end(true);
        actions.push({ type: "closed", reason: closed.reason });
        break;
      }
      case FrameType.Error: {
        const error = this.json<ErrorMessage>(frame);
        if (!KEEPS_ATTACHMENT.has(error.code)) this.end(true);
        actions.push({ type: "error", error });
        break;
      }
      default:
        throw ClientError.protocol(`${type} on an attachment`);
    }
  }

  /** A stream frame at the cursor. */
  private stream(frame: Frame, actions: Action[]): void {
    switch (frame.header.type) {
      case FrameType.Output: {
        const output = parseOutput(frame.payload);
        if (!output) throw ClientError.protocol("short OUTPUT");
        if (output.offset !== this.cursor.offset) {
          throw ClientError.protocol(`OUTPUT at ${output.offset}, expected ${this.cursor.offset}`);
        }
        const term = this.term();
        if (!term) throw ClientError.protocol("OUTPUT before the terminal exists");
        term.write(output.bytes);
        this.cursor.offset += output.bytes.length;
        actions.push({ type: "output", bytes: output.bytes });
        break;
      }
      case FrameType.Resized: {
        const resized = this.json<Resized>(frame);
        this.checkMarker(resized.seq, resized.offset);
        this.term()?.resize(resized.cols, resized.rows);
        actions.push({ type: "resized", cols: resized.cols, rows: resized.rows });
        break;
      }
      case FrameType.Exited: {
        const exited = this.json<Exited>(frame);
        this.checkMarker(exited.seq, exited.offset);
        this.end(false);
        actions.push({ type: "exited", exit: exited.exit });
        break;
      }
    }
  }

  private checkMarker(seq: number, offset: number): void {
    if (offset !== this.cursor.offset || seq !== this.cursor.marker + 1) {
      throw ClientError.protocol(
        `marker ${seq} at ${offset}, expected ${this.cursor.marker + 1} at ${this.cursor.offset}`,
      );
    }
    this.cursor.marker = seq;
  }

  /** Keeps the terminal for display, if there is one. */
  private end(closed: boolean): void {
    let term: Terminal | undefined;
    switch (this.state.kind) {
      case "bootstrapping":
        term = this.state.decoder.release();
        break;
      case "streaming":
      case "ended":
        term = this.state.term;
        break;
    }
    this.state = { kind: "ended", term, closed };
  }

  /** Frees whatever engine objects the state holds. */
  private discard(): void {
    switch (this.state.kind) {
      case "bootstrapping":
        this.state.decoder.free();
        break;
      case "streaming":
      case "ended":
        this.state.term?.free();
        break;
    }
    this.state = { kind: "opening" };
  }
}
