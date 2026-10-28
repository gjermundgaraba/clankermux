import {
  type Cell,
  FLAG,
  type Frame,
  type FrameRow,
  NO_COLOR,
  PACKED_BG,
  PACKED_FG,
  PACKED_FLAGS_MASK,
  PACKED_STRIDE,
  PACKED_STYLE,
  PACKED_UNDERLINE_SHIFT,
  type PackedCells,
  packedText,
} from "../engine/render.ts";

type Cells = Cell[] | PackedCells;

/** The cell grid in CSS pixels and the fonts to draw with. */
export interface Metrics {
  cellWidth: number;
  cellHeight: number;
  /** Distance from the top of a cell to the text baseline. */
  baseline: number;
  font: string;
  boldFont: string;
  italicFont: string;
  boldItalicFont: string;
}

/** CSS colours used where the frame has none: only the cursor, since the
 * frame always carries default foreground and background colours. */
export interface Palette {
  cursor: string;
}

/** The part of `CanvasRenderingContext2D` the renderer uses. */
export interface Context2D {
  fillStyle: string | CanvasGradient | CanvasPattern;
  font: string;
  globalAlpha: number;
  fillRect(x: number, y: number, w: number, h: number): void;
  fillText(text: string, x: number, y: number): void;
}

const hex = (color: number) => `#${color.toString(16).padStart(6, "0")}`;
const css = (color: number, fallback: string) => (color === NO_COLOR ? fallback : hex(color));

const SCROLLBACK_MARK_WIDTH_PX = 3;

/**
 * Draws frames onto a 2D context: changed rows only, then the cursor, then
 * the scrollback mark. Rows are cached so the rows the cursor or the mark
 * leaves can be redrawn.
 */
export class CanvasRenderer {
  private rows = new Map<number, FrameRow<Cells>>();
  private cursorRow: number | undefined;
  private lastScrollbackMark: { top: number; height: number } | undefined;
  private cols = 0;
  private background = "";
  private foreground = "";

  constructor(
    private readonly ctx: Context2D,
    private readonly metrics: Metrics,
    private readonly palette: Palette,
  ) {}

  draw(frame: Frame<Cells>): void {
    this.background = hex(frame.background);
    this.foreground = hex(frame.foreground);
    if (frame.full || frame.cols !== this.cols) {
      this.cols = frame.cols;
      this.rows.clear();
      this.lastScrollbackMark = undefined;
      this.ctx.globalAlpha = 1;
      this.ctx.fillStyle = this.background;
      this.ctx.fillRect(0, 0, frame.cols * this.metrics.cellWidth, frame.rows * this.metrics.cellHeight);
    }
    const drawn = new Set<number>();
    for (const row of frame.lines) {
      this.rows.set(row.y, row);
      this.drawRow(row);
      drawn.add(row.y);
    }
    const mark = this.markOf(frame);
    this.eraseLastMark(drawn);
    const cursor = frame.cursor?.visible ? frame.cursor : undefined;
    this.redrawVacatedCursorRow(cursor, drawn);
    this.cursorRow = undefined;
    if (cursor) {
      if (!drawn.has(cursor.y)) {
        const row = this.rows.get(cursor.y);
        if (row) this.drawRow(row);
      }
      this.drawCursor(cursor);
      this.cursorRow = cursor.y;
    }
    this.lastScrollbackMark = mark;
    if (mark) {
      this.ctx.globalAlpha = 0.5;
      this.ctx.fillStyle = this.foreground;
      this.ctx.fillRect(
        this.cols * this.metrics.cellWidth - SCROLLBACK_MARK_WIDTH_PX,
        mark.top,
        SCROLLBACK_MARK_WIDTH_PX,
        mark.height,
      );
      this.ctx.globalAlpha = 1;
    }
  }

  private eraseLastMark(drawn: Set<number>): void {
    const last = this.lastScrollbackMark;
    if (!last) return;
    const { cellHeight } = this.metrics;
    const end = Math.ceil((last.top + last.height) / cellHeight);
    for (let y = Math.floor(last.top / cellHeight); y < end; y++) {
      const row = this.rows.get(y);
      if (row && !drawn.has(y)) {
        this.drawRow(row);
        drawn.add(y);
      }
    }
  }

  private redrawVacatedCursorRow(cursor: Frame["cursor"], drawn: Set<number>): void {
    const vacated = this.cursorRow;
    if (vacated === undefined || vacated === cursor?.y || drawn.has(vacated)) return;
    const row = this.rows.get(vacated);
    if (row) this.drawRow(row);
  }

  private markOf(frame: Frame<Cells>): { top: number; height: number } | undefined {
    const { total, offset, len } = frame.scrollbar;
    if (offset + len >= total) return undefined;
    const height = frame.rows * this.metrics.cellHeight;
    const size = Math.max(Math.round((height * len) / total), Math.min(height, this.metrics.cellHeight));
    return { top: Math.min(Math.round((height * offset) / total), height - size), height: size };
  }

  private drawRow(row: FrameRow<Cells>): void {
    const { cellWidth, cellHeight, baseline } = this.metrics;
    const y = row.y * cellHeight;
    this.ctx.globalAlpha = 1;
    this.ctx.fillStyle = this.background;
    this.ctx.fillRect(0, y, this.cols * cellWidth, cellHeight);
    const cells = row.cells;
    const count = Array.isArray(cells) ? cells.length : cells.data.length / PACKED_STRIDE;
    for (let x = 0; x < count; x++) {
      let text: string, foreground: number, background: number, flags: number, underline: number;
      if (Array.isArray(cells)) {
        ({ text, fg: foreground, bg: background, flags, underline } = cells[x]!);
      } else {
        const at = x * PACKED_STRIDE;
        text = packedText(cells, x);
        const style = cells.data[at + PACKED_STYLE]!;
        foreground = cells.data[at + PACKED_FG]!;
        background = cells.data[at + PACKED_BG]!;
        flags = style & PACKED_FLAGS_MASK;
        underline = style >>> PACKED_UNDERLINE_SHIFT;
      }
      if (flags & FLAG.SPACER) continue;
      const width = flags & FLAG.WIDE ? 2 : 1;
      let fg = css(foreground, this.foreground);
      let bg = css(background, this.background);
      const selected = row.selection !== undefined && x >= row.selection[0] && x <= row.selection[1];
      const inverted = ((flags & FLAG.INVERSE) !== 0) !== selected;
      if (inverted) [fg, bg] = [bg, fg];
      if (bg !== this.background) {
        this.ctx.fillStyle = bg;
        this.ctx.fillRect(x * cellWidth, y, width * cellWidth, cellHeight);
      }
      if (flags & FLAG.INVISIBLE) continue;
      this.ctx.globalAlpha = flags & FLAG.FAINT ? 0.6 : 1;
      this.ctx.fillStyle = fg;
      if (text !== "" && text !== " ") {
        this.ctx.font = this.font(flags);
        this.ctx.fillText(text, x * cellWidth, y + baseline);
      }
      if (underline !== 0) {
        this.ctx.fillRect(x * cellWidth, y + cellHeight - 2, width * cellWidth, 1);
      }
      if (flags & FLAG.STRIKETHROUGH) {
        this.ctx.fillRect(x * cellWidth, y + Math.floor(cellHeight / 2), width * cellWidth, 1);
      }
    }
    this.ctx.globalAlpha = 1;
  }

  private font(flags: number): string {
    const bold = (flags & FLAG.BOLD) !== 0;
    const italic = (flags & FLAG.ITALIC) !== 0;
    if (bold && italic) return this.metrics.boldItalicFont;
    if (bold) return this.metrics.boldFont;
    if (italic) return this.metrics.italicFont;
    return this.metrics.font;
  }

  private drawCursor(cursor: NonNullable<Frame["cursor"]>): void {
    const { cellWidth, cellHeight, baseline } = this.metrics;
    const x = cursor.x * cellWidth;
    const y = cursor.y * cellHeight;
    const color = css(cursor.color, this.palette.cursor);
    this.ctx.globalAlpha = 1;
    this.ctx.fillStyle = color;
    switch (cursor.style) {
      case "bar":
        this.ctx.fillRect(x, y, 2, cellHeight);
        return;
      case "underline":
        this.ctx.fillRect(x, y + cellHeight - 2, cellWidth, 2);
        return;
      case "block_hollow":
        this.ctx.fillRect(x, y, cellWidth, 1);
        this.ctx.fillRect(x, y + cellHeight - 1, cellWidth, 1);
        this.ctx.fillRect(x, y, 1, cellHeight);
        this.ctx.fillRect(x + cellWidth - 1, y, 1, cellHeight);
        return;
      default: {
        this.ctx.fillRect(x, y, cellWidth, cellHeight);
        const cells = this.rows.get(cursor.y)?.cells;
        let text = "", flags = 0;
        if (Array.isArray(cells)) {
          const cell = cells[cursor.x];
          if (cell) ({ text, flags } = cell);
        } else if (cells && cursor.x < cells.data.length / PACKED_STRIDE) {
          text = packedText(cells, cursor.x);
          flags = cells.data[cursor.x * PACKED_STRIDE + PACKED_STYLE]! & PACKED_FLAGS_MASK;
        }
        if (text !== "" && text !== " ") {
          this.ctx.fillStyle = this.background;
          this.ctx.font = this.font(flags);
          this.ctx.fillText(text, x, y + baseline);
        }
      }
    }
  }
}
