import type { Engine, Terminal } from "./index.ts";

/** A point in the viewport in cells, fractions included: the pointer's
 * place within its cell decides which cells a drag covers. */
export interface CellPoint {
  x: number;
  y: number;
}

const PIXELS_PER_CELL = 1000;
const MULTI_CLICK_INTERVAL_MS = 500;

export class SelectionGesture {
  private gesture: number;
  private readonly events: { press: number; drag: number; release: number };
  private term: number | undefined;

  constructor(readonly engine: Engine) {
    this.gesture = engine.construct("ghostty_selection_gesture_new");
    const event = (type: string) =>
      engine.construct("ghostty_selection_gesture_event_new", engine.enumValue("GhosttySelectionGestureEventType", type));
    this.events = { press: event("PRESS"), drag: event("DRAG"), release: event("RELEASE") };
    const f64 = new Uint8Array(8);
    new DataView(f64.buffer).setFloat64(0, PIXELS_PER_CELL, true);
    this.set(this.events.press, "REPEAT_DISTANCE", f64);
    this.set(this.events.press, "REPEAT_INTERVAL_NS", u64(MULTI_CLICK_INTERVAL_MS * 1e6));
  }

  press(term: Terminal, at: CellPoint, time: number): Uint8Array | undefined {
    this.follow(term);
    this.set(this.events.press, "REF", term.gridRef(at));
    this.set(this.events.press, "POSITION", position(at));
    this.set(this.events.press, "TIME_NS", u64(time * 1e6));
    return this.apply(term, this.events.press);
  }

  drag(term: Terminal, at: CellPoint): Uint8Array | undefined {
    if (this.term !== term.handle) return undefined;
    const geometry = new Uint8Array(16);
    const view = new DataView(geometry.buffer);
    view.setUint32(0, term.cols, true);
    view.setUint32(4, PIXELS_PER_CELL, true);
    view.setUint32(8, 0, true);
    view.setUint32(12, term.rows * PIXELS_PER_CELL, true);
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

  forget(term?: Terminal): void {
    if (this.term === undefined) return;
    this.engine.call("ghostty_selection_gesture_free", this.gesture, term?.handle ?? 0);
    this.gesture = this.engine.construct("ghostty_selection_gesture_new");
    this.term = undefined;
  }

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
      if (!e.present("ghostty_selection_gesture_event", this.gesture, term.handle, event, out)) return undefined;
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

function position(at: CellPoint): Uint8Array {
  const bytes = new Uint8Array(16);
  const view = new DataView(bytes.buffer);
  view.setFloat64(0, at.x * PIXELS_PER_CELL, true);
  view.setFloat64(8, at.y * PIXELS_PER_CELL, true);
  return bytes;
}
