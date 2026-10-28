import { readFileSync } from "node:fs";
import { beforeAll, expect, test } from "vitest";

import { Engine, type Frame, type PackedFrame, RenderState, SelectionGesture } from "../../src/engine/index.ts";

let engine: Engine;
beforeAll(async () => {
  engine = await Engine.load(readFileSync(new URL("../../ghostty-vt.wasm", import.meta.url)));
});

function unpack(frame: PackedFrame): Frame {
  return {
    ...frame,
    lines: frame.lines.map((row) => ({
      ...row,
      cells: Array.from({ length: row.cells.data.length / 4 }, (_, x) => {
        const [code, fg, bg, style] = row.cells.data.subarray(x * 4, x * 4 + 4);
        return {
          text: code! < 0 ? row.cells.graphemes[-code! - 1]! : code === 0 ? "" : String.fromCodePoint(code!),
          fg: fg!, bg: bg!, flags: style! & 0xffff, underline: style! >>> 16,
        };
      }),
    })),
  };
}

for (const [cols, rows] of [[80, 24], [160, 50], [240, 80]] as const) {
  test.each([false, true])(`${cols}x${rows} packed transfers preserve object frames (styled=%s)`, (styled) => {
    const terms = [engine.terminalNew(cols, rows), engine.terminalNew(cols, rows)] as const;
    const objects = new RenderState(engine);
    const packed = new RenderState(engine);
    const compare = () => {
      const expected = objects.update(terms[0]);
      const captured = packed.updatePacked(terms[1]);
      const transfer = captured.lines.map((row) => row.cells.data.buffer);
      const received = structuredClone(captured, { transfer });
      expect(transfer.every((buffer) => buffer.byteLength === 0)).toBe(true);
      expect(unpack(received)).toEqual(expected);
      objects.clean();
      packed.clean();
      return received;
    };
    const write = (text: string) => {
      for (const term of terms) term.write(new TextEncoder().encode(text));
    };
    try {
      const row = (styled ? "\x1b[1;38;2;123;234;45m" : "") + "x".repeat(cols - 1) + "\r\n";
      write("\x1b[H" + row.repeat(rows - 1));
      const first = compare();
      expect(first.lines.reduce((n, row) => n + row.cells.data.byteLength, 0)).toBe(cols * rows * 16);
      expect(first.lines.every((row) => row.cells.graphemes.length === 0)).toBe(true);
      write("\x1b[H\x1b[1;2;3;4;5;7;8;9;53m漢e" + "\u0301".repeat(40) + "👩‍💻\x1b[0m");
      expect(compare().lines.some((row) => row.cells.graphemes.length > 0)).toBe(true);
      for (const term of terms) {
        const gesture = new SelectionGesture(engine);
        gesture.press(term, { x: 0.2, y: 0.5 }, 1000);
        term.select(gesture.drag(term, { x: 5.6, y: 1.5 }));
        gesture.release(term, { x: 5.6, y: 1.5 });
        gesture.free();
      }
      expect(compare().lines.some((row) => row.selection !== undefined)).toBe(true);
      write("\x1b[3;1H");
      compare();
    } finally {
      objects.free();
      packed.free();
      for (const term of terms) term.free();
    }
  });
}
