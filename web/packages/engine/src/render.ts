import { type BitField, type Engine, type Terminal } from "./index.ts";

/** Style flag bits of a [`Cell`]. */
export const FLAG = {
  BOLD: 1,
  ITALIC: 2,
  FAINT: 4,
  BLINK: 8,
  INVERSE: 16,
  INVISIBLE: 32,
  STRIKETHROUGH: 64,
  OVERLINE: 128,
  /** The cell is the left half of a wide character. */
  WIDE: 256,
  /** The cell is the right half of a wide character; draw nothing. */
  SPACER: 512,
} as const;

/** No explicit colour; the renderer uses the default. */
export const NO_COLOR = -1;

export interface Cell {
  /** The grapheme, empty for a blank cell. */
  text: string;
  /** 0xRRGGBB or NO_COLOR. */
  fg: number;
  bg: number;
  flags: number;
  /** `GhosttySgrUnderline`: 0 none, 1 single, 2 double, 3 curly, 4 dotted, 5 dashed. */
  underline: number;
}

/** `GhosttyRenderStateCursorVisualStyle` by name. */
export type CursorStyle = "bar" | "block" | "underline" | "block_hollow";
const CURSOR_STYLES: CursorStyle[] = ["bar", "block", "underline", "block_hollow"];

export interface FrameRow {
  y: number;
  cells: Cell[];
}

export interface Frame {
  cols: number;
  rows: number;
  /** Every row is included and the renderer should clear first. */
  full: boolean;
  background: number;
  foreground: number;
  cursor:
    | {
        x: number;
        y: number;
        visible: boolean;
        blinking: boolean;
        style: CursorStyle;
        color: number;
      }
    | undefined;
  /** The rows that changed, or every row when `full`. */
  lines: FrameRow[];
}

/**
 * The engine's render state: updated from a terminal, read into a
 * [`Frame`] of the rows that changed, then cleaned.
 */
export class RenderState {
  private readonly state: number;
  private readonly iterator: number;
  private readonly cells: number;
  private readonly keys: { data: Record<string, number>; row: Record<string, number>; cell: Record<string, number> };
  private readonly tags: { codepoint: number; grapheme: number };
  private readonly wide: { wide: number; tail: number };
  /** `GhosttyCell` bit fields, from the manifest. */
  private readonly cell: { tag: BitField; codepoint: BitField; wide: BitField };
  private readonly cursorStyles: Map<number, CursorStyle>;

  constructor(readonly engine: Engine) {
    this.state = engine.construct("ghostty_render_state_new");
    this.iterator = engine.construct("ghostty_render_state_row_iterator_new");
    this.cells = engine.construct("ghostty_render_state_row_cells_new");
    const values = (name: string, members: string[]) =>
      Object.fromEntries(members.map((m) => [m, engine.enumValue(name, m)]));
    this.keys = {
      data: values("GhosttyRenderStateData", ["COLS", "ROWS", "DIRTY", "ROW_ITERATOR", "COLORS", "CURSOR"]),
      row: values("GhosttyRenderStateRowData", ["CELLS"]),
      cell: values("GhosttyRenderStateRowCellsData", [
        "RAW",
        "STYLE",
        "BG_COLOR",
        "FG_COLOR",
        "HAS_STYLING",
        "GRAPHEMES_UTF8",
      ]),
    };
    this.tags = {
      codepoint: engine.enumValue("GhosttyCellContentTag", "CODEPOINT"),
      grapheme: engine.enumValue("GhosttyCellContentTag", "CODEPOINT_GRAPHEME"),
    };
    this.wide = {
      wide: engine.enumValue("GhosttyCellWide", "WIDE"),
      tail: engine.enumValue("GhosttyCellWide", "SPACER_TAIL"),
    };
    this.cell = {
      tag: engine.bits("GhosttyCell", "content_tag"),
      codepoint: engine.bits("GhosttyCell", "content", "CODEPOINT", "codepoint"),
      wide: engine.bits("GhosttyCell", "wide"),
    };    this.cursorStyles = new Map(
      CURSOR_STYLES.map((style) => [engine.enumValue("GhosttyRenderStateCursorVisualStyle", style.toUpperCase()), style]),
    );
  }

  /** Captures the terminal and returns the rows that changed since the last `clean`. */
  update(term: Terminal): Frame {
    const e = this.engine;
    e.check("ghostty_render_state_update", this.state, term.handle);
    const u16 = (key: number) =>
      e.scratch(4, (out) => {
        e.check("ghostty_render_state_get", this.state, key, out);
        return e.view.getUint16(out, true);
      });
    const cols = u16(this.keys.data["COLS"]!);
    const rows = u16(this.keys.data["ROWS"]!);
    const dirty = e.scratch(4, (out) => {
      e.check("ghostty_render_state_get", this.state, this.keys.data["DIRTY"]!, out);
      return e.view.getInt32(out, true);
    });
    const full = dirty === e.enumValue("GhosttyRenderStateDirty", "FULL");

    const colors = e.pack("GhosttyRenderStateColors", {});
    const colorsPtr = e.copyIn(colors);
    let background: number;
    let foreground: number;
    let cursorColor: number;
    try {
      e.check("ghostty_render_state_get", this.state, this.keys.data["COLORS"]!, colorsPtr);
      background = e.readRgb(colorsPtr + e.offsetOf("GhosttyRenderStateColors", "background"));
      foreground = e.readRgb(colorsPtr + e.offsetOf("GhosttyRenderStateColors", "foreground"));
      cursorColor = e.field("GhosttyRenderStateColors", colorsPtr, "cursor_has_value")
        ? e.readRgb(colorsPtr + e.offsetOf("GhosttyRenderStateColors", "cursor"))
        : NO_COLOR;
    } finally {
      e.free(colorsPtr, colors.length);
    }

    const cursorBytes = e.pack("GhosttyRenderStateCursor", {});
    const cursorPtr = e.copyIn(cursorBytes);
    let cursor: Frame["cursor"];
    try {
      e.check("ghostty_render_state_get", this.state, this.keys.data["CURSOR"]!, cursorPtr);
      const f = (key: string) => e.field("GhosttyRenderStateCursor", cursorPtr, key);
      cursor = f("viewport_has_value")
        ? {
            x: f("viewport_x"),
            y: f("viewport_y"),
            visible: f("visible") !== 0,
            blinking: f("blinking") !== 0,
            style: this.cursorStyles.get(f("visual_style")) ?? "block",
            color: cursorColor,
          }
        : undefined;
    } finally {
      e.free(cursorPtr, cursorBytes.length);
    }

    const lines: FrameRow[] = [];
    // Iterator and cells containers are passed by address of their handle.
    e.scratch(8, (out) => {
      e.view.setUint32(out, this.iterator, true);
      e.check("ghostty_render_state_get", this.state, this.keys.data["ROW_ITERATOR"]!, out);
      while (e.call("ghostty_render_state_row_iterator_next_dirty", this.iterator, out + 4)) {
        const y = e.view.getUint16(out + 4, true);
        lines.push({ y, cells: this.readRow(cols) });
      }
    });
    return { cols, rows, full, background, foreground, cursor, lines };
  }

  private readRow(cols: number): Cell[] {
    const e = this.engine;
    e.scratch(4, (out) => {
      e.view.setUint32(out, this.cells, true);
      e.check("ghostty_render_state_row_get", this.iterator, this.keys.row["CELLS"]!, out);
    });
    const styleSize = e.sizeOf("GhosttyStyle");
    const cells: Cell[] = [];
    // Scratch: raw cell (8), colour (3, padded to 8), the 8-aligned style,
    // a UTF-8 buffer struct and its bytes.
    const utf8Cap = 64;
    e.scratch(16 + styleSize + e.sizeOf("GhosttyBuffer") + utf8Cap, (scratch) => {
      const raw = scratch;
      const color = scratch + 8;
      const style = scratch + 16;
      const buffer = style + styleSize;
      const utf8 = buffer + e.sizeOf("GhosttyBuffer");
      while (e.call("ghostty_render_state_row_cells_next", this.cells) && cells.length < cols) {
        e.check("ghostty_render_state_row_cells_get", this.cells, this.keys.cell["RAW"]!, raw);
        const tag = e.readBits(raw, this.cell.tag);
        const wide = e.readBits(raw, this.cell.wide);
        let text = "";
        if (tag === this.tags.codepoint) {
          const cp = e.readBits(raw, this.cell.codepoint);
          if (cp !== 0) text = String.fromCodePoint(cp);
        } else if (tag === this.tags.grapheme) {
          text = this.graphemes(buffer, utf8, utf8Cap);
        }
        let flags = 0;
        let underline = 0;
        if (wide === this.wide.wide) flags |= FLAG.WIDE;
        if (wide === this.wide.tail) flags |= FLAG.SPACER;
        const fg = this.color(this.keys.cell["FG_COLOR"]!, color);
        const bg = this.color(this.keys.cell["BG_COLOR"]!, color);
        e.check("ghostty_render_state_row_cells_get", this.cells, this.keys.cell["HAS_STYLING"]!, raw);
        if (e.memory[raw]) {
          e.view.setUint32(style, styleSize, true);
          e.check("ghostty_render_state_row_cells_get", this.cells, this.keys.cell["STYLE"]!, style);
          const f = (key: string) => e.field("GhosttyStyle", style, key) !== 0;
          if (f("bold")) flags |= FLAG.BOLD;
          if (f("italic")) flags |= FLAG.ITALIC;
          if (f("faint")) flags |= FLAG.FAINT;
          if (f("blink")) flags |= FLAG.BLINK;
          if (f("inverse")) flags |= FLAG.INVERSE;
          if (f("invisible")) flags |= FLAG.INVISIBLE;
          if (f("strikethrough")) flags |= FLAG.STRIKETHROUGH;
          if (f("overline")) flags |= FLAG.OVERLINE;
          underline = e.field("GhosttyStyle", style, "underline");
        }
        cells.push({ text, fg, bg, flags, underline });
      }
    });
    return cells;
  }

  private color(key: number, out: number): number {
    const e = this.engine;
    const code = e.call("ghostty_render_state_row_cells_get", this.cells, key, out);
    if (code === e.enumValue("GhosttyResult", "INVALID_VALUE")) return NO_COLOR;
    if (code !== e.result.SUCCESS) throw new Error(`cell colour: ${code}`);
    return this.engine.readRgb(out);
  }

  private graphemes(buffer: number, utf8: number, cap: number): string {
    const e = this.engine;
    e.view.setUint32(buffer + e.offsetOf("GhosttyBuffer", "ptr"), utf8, true);
    e.view.setUint32(buffer + e.offsetOf("GhosttyBuffer", "cap"), cap, true);
    e.view.setUint32(buffer + e.offsetOf("GhosttyBuffer", "len"), 0, true);
    const code = e.call("ghostty_render_state_row_cells_get", this.cells, this.keys.cell["GRAPHEMES_UTF8"]!, buffer);
    if (code === e.result.SUCCESS) {
      const len = e.field("GhosttyBuffer", buffer, "len");
      return new TextDecoder().decode(e.memory.slice(utf8, utf8 + len));
    }
    if (code !== e.result.OUT_OF_SPACE) throw new Error(`cell graphemes: ${code}`);
    // The buffer struct reports the needed length; retry through it.
    const needed = e.field("GhosttyBuffer", buffer, "len");
    return new TextDecoder().decode(
      e.growing("ghostty_render_state_row_cells_get", needed, (big, bigCap, len) => {
        e.view.setUint32(buffer + e.offsetOf("GhosttyBuffer", "ptr"), big, true);
        e.view.setUint32(buffer + e.offsetOf("GhosttyBuffer", "cap"), bigCap, true);
        const again = e.call("ghostty_render_state_row_cells_get", this.cells, this.keys.cell["GRAPHEMES_UTF8"]!, buffer);
        e.view.setUint32(len, e.field("GhosttyBuffer", buffer, "len"), true);
        return again;
      }),
    );
  }

  /** Marks the frame as drawn. */
  clean(): void {
    this.engine.check("ghostty_render_state_clean", this.state);
  }

  free(): void {
    this.engine.call("ghostty_render_state_row_cells_free", this.cells);
    this.engine.call("ghostty_render_state_row_iterator_free", this.iterator);
    this.engine.call("ghostty_render_state_free", this.state);
  }
}
