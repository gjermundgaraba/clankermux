import { readFileSync } from "node:fs";
import { beforeAll, expect, test } from "vitest";

import { type BitField, Engine, RenderState } from "../../src/engine/index.ts";

const wasm = readFileSync(new URL("../../ghostty-vt.wasm", import.meta.url));
let engine: Engine;
let original: Engine;
const utf8 = (text: string) => new TextEncoder().encode(text);
const readBigBits = (e: Engine, ptr: number, field: BitField): number =>
  Number((e.view.getBigUint64(ptr, true) >> BigInt(field.lsb)) & ((1n << BigInt(field.width)) - 1n));

beforeAll(async () => {
  engine = await Engine.load(wasm);
  original = await Engine.load(wasm);
  original.readBits = (ptr, field) => readBigBits(original, ptr, field);
});

test("packed fields match the 64-bit reader at every word boundary and width", () => {
  engine.scratch(8, (ptr) => {
    for (const value of [0n, 0xffffffffffffffffn, 0x8000000080000000n, 0x12345678abcdef01n]) {
      engine.view.setBigUint64(ptr, value, true);
      for (let lsb = 0; lsb < 64; lsb++) {
        for (let width = 0; width <= 64 - lsb; width++) {
          const field = { lsb, width };
          expect(engine.readBits(ptr, field), `${value.toString(16)} at ${lsb}:${width}`).toBe(readBigBits(engine, ptr, field));
        }
      }
    }
  });
});

for (const [cols, rows] of [[80, 24], [160, 50], [240, 80]] as const) {
  test.each([false, true])(`${cols}x${rows} frames match the BigInt path (styled=%s)`, (styled) => {
    const term = engine.terminalNew(cols, rows);
    const oldTerm = original.terminalNew(cols, rows);
    const render = new RenderState(engine);
    const oldRender = new RenderState(original);
    const row = (styled ? "\x1b[1;38;2;123;234;45m" : "") + "x".repeat(cols - 1) + "\r\n";
    // The review's full-dirty grids, then a partial update with wide and
    // combining characters and every boolean style bit.
    for (const text of [
      "\x1b[H" + row.repeat(rows - 1),
      "\x1b[H\x1b[1;2;3;4;5;7;8;9;53;48;2;10;20;30m漢e\u0301👩‍💻\x1b[0m",
    ]) {
      term.write(utf8(text));
      oldTerm.write(utf8(text));
      expect(render.update(term)).toEqual(oldRender.update(oldTerm));
      render.clean();
      oldRender.clean();
    }
    render.free();
    oldRender.free();
    term.free();
    oldTerm.free();
  });
}
