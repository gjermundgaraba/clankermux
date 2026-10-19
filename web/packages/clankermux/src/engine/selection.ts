import type { Engine, Terminal } from "./index.ts";

/** A point in the viewport in cells, fractions included: the pointer's
 * place within its cell decides which cells a drag covers. */
export interface CellPoint {
  x: number;
  y: number;
}

/** Pixels per cell in the gesture's geometry: positions are in cells,
 * scaled to whole numbers. */
const SCALE = 1000;
/** Clicks closer together than this, and within a cell of each other,
 * count up to a word (two) and a line (three). */
const REPEAT_MS = 500;

/**
 * Mouse selection through the engine's gesture: a press, then drags, then
 * a release. A single press selects nothing; a drag selects from it by
 * cells, a double press a word and a triple press a line, dragging by
 * words or lines. The gesture tracks its anchor in the terminal, so
 * output between press and drag does not move it.
 */
export class SelectionGesture {
  private gesture: number;
  private readonly events: { press: number; drag: number; release: number };
  /** The terminal the gesture tracks its anchor in. */
  private term: number | undefined;

  constructor(readonly engine: Engine) {
    this.gesture = engine.construct("ghostty_selection_gesture_new");
    const event = (type: string) =>
      engine.construct("ghostty_selection_gesture_event_new", engine.enumValue("GhosttySelectionGestureEventType", type));
    this.events = { press: event("PRESS"), drag: event("DRAG"), release: event("RELEASE") };
    const f64 = new Uint8Array(8);
    new DataView(f64.buffer).setFloat64(0, SCALE, true);
    this.set(this.events.press, "REPEAT_DISTANCE", f64);
    this.set(this.events.press, "REPEAT_INTERVAL_NS", u64(REPEAT_MS * 1e6));
  }

  /** Presses at `at` (`time` in ms); the selection to install, or
   * undefined for none (a single press). */
  press(term: Terminal, at: CellPoint, time: number): Uint8Array | undefined {
    this.follow(term);
    this.set(this.events.press, "REF", term.gridRef(at));
    this.set(this.events.press, "POSITION", position(at));
    this.set(this.events.press, "TIME_NS", u64(time * 1e6));
    return this.apply(term, this.events.press);
  }

  /** Drags to `at`; the selection from the press, or undefined when the
   * pointer has not left the pressed cell. */
  drag(term: Terminal, at: CellPoint): Uint8Array | undefined {
    if (this.term !== term.handle) return undefined;
    const geometry = new Uint8Array(16);
    const view = new DataView(geometry.buffer);
    view.setUint32(0, term.cols, true);
    view.setUint32(4, SCALE, true);
    view.setUint32(8, 0, true);
    view.setUint32(12, term.rows * SCALE, true);
    this.set(this.events.drag, "REF", term.gridRef(at));
    this.set(this.events.drag, "POSITION", position(at));
    this.set(this.events.drag, "GEOMETRY", geometry);
    return this.apply(term, this.events.drag);
  }

  release(term: Terminal, at: CellPoint): void {
    if (this.term !== term.handle) return;
    this.set(this.events.release, "REF", term.gridRef(at));
    this.apply(term, this.events.release);
  }

  /** Starts afresh, for a terminal freed or about to be: `term` when it
   * is still alive, to release the tracked anchor in it, else none. The
   * caller says so, since a new terminal may have the handle of a freed
   * one. */
  forget(term?: Terminal): void {
    if (this.term === undefined) return;
    this.engine.call("ghostty_selection_gesture_free", this.gesture, term?.handle ?? 0);
    this.gesture = this.engine.construct("ghostty_selection_gesture_new");
    this.term = undefined;
  }

  /** A gesture is in one terminal. */
  private follow(term: Terminal): void {
    if (this.term !== term.handle) this.forget();
    this.term = term.handle;
  }

  private set(event: number, option: string, value: Uint8Array): void {
    const e = this.engine;
    e.withBytes(value, (ptr) =>
      e.check("ghostty_selection_gesture_event_set", event, e.enumValue("GhosttySelectionGestureEventOption", option), ptr),
    );
  }

  private apply(term: Terminal, event: number): Uint8Array | undefined {
    const e = this.engine;
    const size = e.sizeOf("GhosttySelection");
    return e.scratch(size, (out) => {
      e.memory.fill(0, out, out + size);
      e.view.setUint32(out, size, true);
      const code = e.call("ghostty_selection_gesture_event", this.gesture, term.handle, event, out);
      if (code === e.result.NO_VALUE) return undefined;
      if (code !== e.result.SUCCESS) throw new Error(`selection gesture: ${code}`);
      return e.memory.slice(out, out + size);
    });
  }

  free(): void {
    for (const event of Object.values(this.events)) this.engine.call("ghostty_selection_gesture_event_free", event);
    this.engine.call("ghostty_selection_gesture_free", this.gesture, 0);
  }
}

function u64(value: number): Uint8Array {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setBigUint64(0, BigInt(Math.round(value)), true);
  return bytes;
}

/** A `GhosttySurfacePosition` for `at`, in the gesture's scaled pixels. */
function position(at: CellPoint): Uint8Array {
  const bytes = new Uint8Array(16);
  const view = new DataView(bytes.buffer);
  view.setFloat64(0, at.x * SCALE, true);
  view.setFloat64(8, at.y * SCALE, true);
  return bytes;
}
