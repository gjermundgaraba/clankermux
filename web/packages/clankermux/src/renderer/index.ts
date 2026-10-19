import { FLAG, type Frame, type FrameRow, NO_COLOR } from "../engine/index.ts";

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
/** A cell or cursor colour, or `fallback` when it has none. */
const css = (color: number, fallback: string) => (color === NO_COLOR ? fallback : hex(color));

/** Width in CSS pixels of the mark at the right edge that shows the view
 * scrolled back, and where it is in the history. */
const MARK_WIDTH = 3;

/**
 * Draws frames onto a 2D context: changed rows only, then the cursor, then
 * the scrollback mark. Rows are cached so the rows the cursor or the mark
 * leaves can be redrawn.
 */
export class CanvasRenderer {
  private rows = new Map<number, FrameRow>();
  private cursorRow: number | undefined;
  /** The scrollback mark drawn last, in pixels from the top. */
  private mark: { top: number; height: number } | undefined;
  private cols = 0;
  private background = "";
  private foreground = "";

  constructor(
    private readonly ctx: Context2D,
    private readonly metrics: Metrics,
    private readonly palette: Palette,
  ) {}

  draw(frame: Frame): void {
    // The frame always carries the default colours.
    this.background = hex(frame.background);
    this.foreground = hex(frame.foreground);
    if (frame.full || frame.cols !== this.cols) {
      this.cols = frame.cols;
      this.rows.clear();
      this.mark = undefined;
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
    // The last mark is erased with the rows under it, every frame: it is
    // translucent, so one drawn over itself would darken.
    const mark = this.markOf(frame);
    if (this.mark) {
      const { cellHeight } = this.metrics;
      const last = Math.ceil((this.mark.top + this.mark.height) / cellHeight);
      for (let y = Math.floor(this.mark.top / cellHeight); y < last; y++) {
        const row = this.rows.get(y);
        if (row && !drawn.has(y)) {
          this.drawRow(row);
          drawn.add(y);
        }
      }
    }
    // The cursor's old row is redrawn from the cache when the cursor
    // moved or went away; the engine marks no row dirty for either.
    const cursor = frame.cursor?.visible ? frame.cursor : undefined;
    if (this.cursorRow !== undefined && this.cursorRow !== cursor?.y && !drawn.has(this.cursorRow)) {
      const row = this.rows.get(this.cursorRow);
      if (row) this.drawRow(row);
    }
    this.cursorRow = undefined;
    if (cursor) {
      if (!drawn.has(cursor.y)) {
        const row = this.rows.get(cursor.y);
        if (row) this.drawRow(row);
      }
      this.drawCursor(cursor);
      this.cursorRow = cursor.y;
    }
    // Over everything, on rows just drawn.
    this.mark = mark;
    if (mark) {
      this.ctx.globalAlpha = 0.5;
      this.ctx.fillStyle = this.foreground;
      this.ctx.fillRect(this.cols * this.metrics.cellWidth - MARK_WIDTH, mark.top, MARK_WIDTH, mark.height);
      this.ctx.globalAlpha = 1;
    }
  }

  /** The scrollback mark for `frame`: the viewport's share of the history
   * at the right edge, while the view is scrolled back. */
  private markOf(frame: Frame): { top: number; height: number } | undefined {
    const { total, offset, len } = frame.scrollbar;
    if (offset + len >= total) return undefined;
    const height = frame.rows * this.metrics.cellHeight;
    const size = Math.max(Math.round((height * len) / total), Math.min(height, this.metrics.cellHeight));
    return { top: Math.min(Math.round((height * offset) / total), height - size), height: size };
  }

  private drawRow(row: FrameRow): void {
    const { cellWidth, cellHeight, baseline } = this.metrics;
    const y = row.y * cellHeight;
    this.ctx.globalAlpha = 1;
    this.ctx.fillStyle = this.background;
    this.ctx.fillRect(0, y, this.cols * cellWidth, cellHeight);
    for (let x = 0; x < row.cells.length; x++) {
      const cell = row.cells[x]!;
      if (cell.flags & FLAG.SPACER) continue;
      const width = cell.flags & FLAG.WIDE ? 2 : 1;
      let fg = css(cell.fg, this.foreground);
      let bg = css(cell.bg, this.background);
      // A selected cell is drawn inverted, an inverse one the other way.
      const selected = row.selection !== undefined && x >= row.selection[0] && x <= row.selection[1];
      if (((cell.flags & FLAG.INVERSE) !== 0) !== selected) [fg, bg] = [bg, fg];
      if (bg !== this.background) {
        this.ctx.fillStyle = bg;
        this.ctx.fillRect(x * cellWidth, y, width * cellWidth, cellHeight);
      }
      if (cell.flags & FLAG.INVISIBLE) continue;
      this.ctx.globalAlpha = cell.flags & FLAG.FAINT ? 0.6 : 1;
      this.ctx.fillStyle = fg;
      if (cell.text !== "" && cell.text !== " ") {
        this.ctx.font = this.font(cell.flags);
        this.ctx.fillText(cell.text, x * cellWidth, y + baseline);
      }
      if (cell.underline !== 0) {
        this.ctx.fillRect(x * cellWidth, y + cellHeight - 2, width * cellWidth, 1);
      }
      if (cell.flags & FLAG.STRIKETHROUGH) {
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
        // The cell's text in the background colour, over the block.
        const cell = this.rows.get(cursor.y)?.cells[cursor.x];
        if (cell && cell.text !== "" && cell.text !== " ") {
          this.ctx.fillStyle = this.background;
          this.ctx.font = this.font(cell.flags);
          this.ctx.fillText(cell.text, x, y + baseline);
        }
      }
    }
  }
}
