import { Decoder, type Engine, type Terminal } from "../engine/index.ts";
import {
  type Attached,
  type Closed,
  type ErrorMessage,
  type Exit,
  type Exited,
  type Frame,
  FrameType,
  type Resized,
  type Resume,
  type Session,
  type Welcome,
  parseOutput,
} from "../proto/index.ts";

import { ClientError } from "./connection.ts";

export type Action =
  | { type: "screen" }
  | { type: "resumed" }
  | { type: "output"; bytes: Uint8Array }
  | { type: "resized"; cols: number; rows: number }
  | { type: "exited"; exit: Exit }
  | { type: "gap" }
  | { type: "closed"; reason: Closed["reason"]; message?: string }
  | { type: "error"; error: ErrorMessage };

type State =
  | { kind: "opening" }
  | { kind: "bootstrapping"; decoder: Decoder; held: Frame[]; heldBytes: number }
  | { kind: "streaming"; term: Terminal }
  | { kind: "ended"; term: Terminal | undefined; serverClosed: boolean };

const KEEPS_ATTACHMENT = new Set<ErrorMessage["code"]>(["read_only", "not_running", "input_overflow"]);

const MAX_HELD_BYTES = 4 * 1024 * 1024;
const HELD_FRAME_OVERHEAD_BYTES = 64;

function mustHoldUntilSnapshotEnd(held: readonly Frame[], type: FrameType): boolean {
  return held.length > 0 || type !== FrameType.Output;
}

export class Attachment {
  private state: State = { kind: "opening" };
  private cursor = { offset: 0, marker: 0 };
  private incarnation = "";
  private continuation = 0;
  private pendingServer: { incarnation: string; continuation: number } | undefined;
  session: Session | undefined;

  constructor(readonly engine: Engine) {}

  openOn(welcome: Welcome): void {
    this.pendingServer = { incarnation: welcome.incarnation, continuation: welcome.limits.continuation };
  }

  resume(): Resume | undefined {
    return this.state.kind === "streaming"
      ? { incarnation: this.incarnation, offset: this.cursor.offset, marker: this.cursor.marker }
      : undefined;
  }

  ended(): boolean {
    return this.state.kind === "ended";
  }

  closed(): boolean {
    return this.state.kind === "ended" && this.state.serverClosed;
  }

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

  free(): void {
    this.discard();
    this.state = { kind: "ended", term: undefined, serverClosed: true };
  }

  private json<T>(frame: Frame): T {
    try {
      return JSON.parse(new TextDecoder().decode(frame.payload)) as T;
    } catch (err) {
      throw ClientError.protocol(`malformed ${frame.header.type}: ${err}`);
    }
  }

  private assertWellFormed(frame: Frame): void {
    this.json<unknown>(frame);
  }

  private step(frame: Frame, actions: Action[]): void {
    const type = frame.header.type;
    switch (type) {
      case FrameType.Attached: {
        const attached = this.json<Attached>(frame);
        if (this.pendingServer) {
          this.incarnation = this.pendingServer.incarnation;
          this.continuation = this.pendingServer.continuation;
          this.pendingServer = undefined;
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
          this.state = { kind: "bootstrapping", decoder, held: [], heldBytes: 0 };
        } else if (this.state.kind === "streaming") {
          actions.push({ type: "resumed" });
        } else {
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
        if (!decoder.terminal()) throw ClientError.protocol("SNAPSHOT_END before READY");
        while (decoder.stepAvailable() && decoder.nextPage()) {}
        if (!decoder.finished()) throw ClientError.protocol("SNAPSHOT_END before FINISH");
        const term = decoder.release()!;
        this.state = { kind: "streaming", term };
        for (const f of held) this.step(f, actions);
        break;
      }
      case FrameType.Output:
      case FrameType.Resized:
      case FrameType.Exited:
        if (this.state.kind === "bootstrapping" && mustHoldUntilSnapshotEnd(this.state.held, type)) {
          const bytes = frame.payload.byteLength + HELD_FRAME_OVERHEAD_BYTES;
          if (bytes > MAX_HELD_BYTES - this.state.heldBytes) {
            this.end(false);
            throw ClientError.desync("bootstrap stream buffer full; fresh OPEN required");
          }
          this.state.heldBytes += bytes;
          this.state.held.push(frame);
          return;
        }
        if (this.state.kind !== "bootstrapping" && this.state.kind !== "streaming") {
          throw ClientError.protocol(`${type} outside the stream`);
        }
        this.stream(frame, actions);
        break;
      case FrameType.Gap:
        this.assertWellFormed(frame);
        this.discard();
        actions.push({ type: "gap" });
        break;
      case FrameType.Closed: {
        const closed = this.json<Closed>(frame);
        this.end(true);
        actions.push({ type: "closed", reason: closed.reason, ...(closed.message !== undefined && { message: closed.message }) });
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

  private stream(frame: Frame, actions: Action[]): void {
    switch (frame.header.type) {
      case FrameType.Output: {
        const output = parseOutput(frame.payload);
        if (!output) throw ClientError.protocol("short OUTPUT");
        if (output.offset !== this.cursor.offset) {
          throw ClientError.desync(`OUTPUT at ${output.offset}, expected ${this.cursor.offset}`);
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
      throw ClientError.desync(
        `marker ${seq} at ${offset}, expected ${this.cursor.marker + 1} at ${this.cursor.offset}`,
      );
    }
    this.cursor.marker = seq;
  }

  private end(serverClosed: boolean): void {
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
    this.state = { kind: "ended", term, serverClosed };
  }

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
