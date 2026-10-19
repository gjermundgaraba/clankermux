import { expect, test } from "vitest";

import { type Cell, FLAG, type Frame, NO_COLOR } from "../../src/engine/index.ts";

import { CanvasRenderer, type Context2D, type Metrics } from "../../src/renderer/index.ts";

type Op = ["rect", string, number, number, number, number] | ["text", string, string, number, number];

/** Records what is drawn. */
class Recording implements Context2D {
  fillStyle = "";
  font = "";
  globalAlpha = 1;
  ops: Op[] = [];
  fillRect(x: number, y: number, w: number, h: number): void {
    this.ops.push(["rect", this.fillStyle, x, y, w, h]);
  }
  fillText(text: string, x: number, y: number): void {
    this.ops.push(["text", this.font, text, x, y]);
  }
  texts(): string[] {
    return this.ops.filter((op) => op[0] === "text").map((op) => op[2] as string);
  }
}

const metrics: Metrics = {
  cellWidth: 10,
  cellHeight: 20,
  baseline: 15,
  font: "r",
  boldFont: "b",
  italicFont: "i",
  boldItalicFont: "bi",
};
const palette = { cursor: "#0f0" };

const blank = (): Cell => ({ text: "", fg: NO_COLOR, bg: NO_COLOR, flags: 0, underline: 0 });
const cell = (text: string, extra: Partial<Cell> = {}): Cell => ({ ...blank(), text, ...extra });
const live = { total: 2, offset: 0, len: 2 };
const frame = (lines: Frame["lines"], cursor: Frame["cursor"], full = false, scrollbar = live): Frame => ({
  cols: 3,
  rows: 2,
  full,
  background: 0x000000,
  foreground: 0xffffff,
  cursor,
  lines,
  scrollbar,
});

test("a full frame clears, rows draw their cells, and the cursor moves between frames", () => {
  const ctx = new Recording();
  const r = new CanvasRenderer(ctx, metrics, palette);
  r.draw(
    frame(
      [
        { y: 0, cells: [cell("a", { flags: FLAG.BOLD, fg: 0xff0000 }), cell("漢", { flags: FLAG.WIDE }), cell("", { flags: FLAG.SPACER })] },
        { y: 1, cells: [cell("b", { bg: 0x0000ff }), cell(" "), blank()] },
      ],
      { x: 0, y: 1, visible: true, blinking: false, style: "block", color: NO_COLOR },
      true,
    ),
  );
  expect(ctx.ops[0]).toEqual(["rect", "#000000", 0, 0, 30, 40]);
  expect(ctx.ops).toContainEqual(["text", "b", "a", 0, 15]);
  expect(ctx.ops).toContainEqual(["text", "r", "漢", 10, 15]);
  expect(ctx.ops).toContainEqual(["rect", "#0000ff", 0, 20, 10, 20]);
  // The block cursor, then the cell's text over it in the background colour.
  expect(ctx.ops.slice(-2)).toEqual([
    ["rect", "#0f0", 0, 20, 10, 20],
    ["text", "r", "b", 0, 35],
  ]);
  expect(ctx.texts().filter((t) => t === " ")).toEqual([]);

  // Only row 0 changed, but the cursor left row 1: that row is redrawn
  // from the cache without the cursor, and the cursor drawn on row 0.
  ctx.ops = [];
  r.draw(frame([{ y: 0, cells: [cell("c"), blank(), blank()] }], { x: 2, y: 0, visible: true, blinking: false, style: "bar", color: 0x00ff00 }));
  expect(ctx.ops[0]).toEqual(["rect", "#000000", 0, 0, 30, 20]);
  expect(ctx.texts()).toEqual(["c", "b"]);
  expect(ctx.ops.at(-1)).toEqual(["rect", "#00ff00", 20, 0, 2, 20]);

  // Hiding the cursor on its own row erases it: row 0 is redrawn from
  // the cache without the cursor. Inverse swaps the colours.
  ctx.ops = [];
  r.draw(frame([{ y: 1, cells: [cell("x", { flags: FLAG.INVERSE }), blank(), blank()] }], { x: 2, y: 0, visible: false, blinking: false, style: "bar", color: 0x00ff00 }));
  expect(ctx.ops).toContainEqual(["rect", "#ffffff", 0, 20, 10, 20]);
  expect(ctx.texts()).toEqual(["x", "c"]);
  expect(ctx.ops.filter((op) => op[1] === "#0f0" || op[1] === "#00ff00")).toEqual([]);
  // Nothing to erase the next time.
  ctx.ops = [];
  r.draw(frame([], { x: 2, y: 0, visible: false, blinking: false, style: "bar", color: 0x00ff00 }));
  expect(ctx.ops).toEqual([]);
});

test("a view scrolled back has a mark at the right edge, moved and erased with the rows under it", () => {
  const ctx = new Recording();
  const r = new CanvasRenderer(ctx, metrics, palette);
  const rows = [
    { y: 0, cells: [cell("a"), blank(), blank()] },
    { y: 1, cells: [cell("b"), blank(), blank()] },
  ];
  // 2 of 8 rows, at the top of the history: the mark is a quarter of the
  // 40px grid, at least a row high.
  r.draw(frame(rows, undefined, true, { total: 8, offset: 0, len: 2 }));
  expect(ctx.ops.at(-1)).toEqual(["rect", "#ffffff", 27, 0, 3, 20]);
  expect(ctx.globalAlpha).toBe(1);

  // Output while scrolled back moves no row, but the history grew and the
  // view moved within it: row 0 under the old mark is redrawn first.
  ctx.ops = [];
  r.draw(frame([], undefined, false, { total: 16, offset: 8, len: 2 }));
  expect(ctx.texts()).toEqual(["a"]);
  expect(ctx.ops.at(-1)).toEqual(["rect", "#ffffff", 27, 20, 3, 20]);

  // Unchanged: drawn again over a row drawn under it.
  ctx.ops = [];
  r.draw(frame([{ y: 1, cells: [cell("c"), blank(), blank()] }], undefined, false, { total: 16, offset: 8, len: 2 }));
  expect(ctx.texts()).toEqual(["c"]);
  expect(ctx.ops.at(-1)).toEqual(["rect", "#ffffff", 27, 20, 3, 20]);

  // Back at the live screen without a full frame: the rows under it are
  // redrawn and no mark is drawn.
  ctx.ops = [];
  r.draw(frame([], undefined, false, { total: 16, offset: 14, len: 2 }));
  expect(ctx.texts()).toEqual(["c"]);
  expect(ctx.ops.filter((op) => op[0] === "rect" && op[2] === 27)).toEqual([]);
});
