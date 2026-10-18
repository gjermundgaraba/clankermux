import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { beforeAll, expect, test } from "vitest";

import { Decoder, Engine, KeyEncoder, type KeyInput, RenderState, keyName } from "../src/index.ts";
import { readyEnd, stepEnd } from "../src/decoder.ts";
import { FLAG, NO_COLOR } from "../src/render.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
let engine: Engine;
beforeAll(async () => {
  engine = await Engine.load(readFileSync(`${here}../ghostty-vt.wasm`));
});

const utf8 = (s: string) => new TextEncoder().encode(s);
const text = (bytes: Uint8Array) => new TextDecoder().decode(bytes);

test("the decoder steps through a snapshot as its bytes arrive", () => {
  const bytes = readFileSync(`${here}fixtures/history.ghostsnp`);
  const decoder = new Decoder(engine, bytes.length);
  decoder.setMaxContinuationBytes(1 << 20);
  const ready = readyEnd(bytes)!;
  decoder.push(bytes.subarray(0, ready - 1));
  expect(decoder.readyAvailable()).toBe(false);
  decoder.push(bytes.subarray(ready - 1, ready));
  expect(decoder.readyAvailable()).toBe(true);
  const term = decoder.ready();
  expect([term.cols, term.rows]).toEqual([20, 5]);
  const before = term.scrollbackRows;
  expect(decoder.stepAvailable()).toBe(false);
  // One page, then FINISH.
  const page = stepEnd(bytes, ready)!;
  decoder.push(bytes.subarray(ready, page));
  expect(decoder.stepAvailable()).toBe(true);
  expect(decoder.nextPage()).toBe(true);
  expect(term.scrollbackRows).toBeGreaterThan(before);
  decoder.push(bytes.subarray(page));
  expect(decoder.nextPage()).toBe(false);
  expect(decoder.stepAvailable()).toBe(false);
  const released = decoder.release()!;
  expect(released.formatPlain()).toBe(readFileSync(`${here}fixtures/history.txt`, "utf8"));
  released.free();
});

test("the decoder refuses more bytes than declared", () => {
  const decoder = new Decoder(engine, 4);
  expect(() => decoder.push(new Uint8Array(5))).toThrow(/declared/);
  decoder.free();
});

test("render state reports changed rows, styles, wide cells and the cursor", () => {
  const term = engine.terminalNew(6, 2);
  const state = new RenderState(engine);
  term.write(utf8("\x1b[1;38;2;255;0;0mhi\x1b[0m 漢"));
  let frame = state.update(term);
  expect([frame.cols, frame.rows, frame.full]).toEqual([6, 2, true]);
  expect(frame.lines.map((r) => r.y)).toEqual([0, 1]);
  const row = frame.lines[0]!.cells;
  expect(row.map((c) => c.text)).toEqual(["h", "i", " ", "漢", "", ""]);
  expect(row[0]).toMatchObject({ fg: 0xff0000, bg: NO_COLOR, flags: FLAG.BOLD, underline: 0 });
  expect(row[2]).toMatchObject({ fg: NO_COLOR, flags: 0 });
  expect(row[3]!.flags & FLAG.WIDE).toBe(FLAG.WIDE);
  expect(row[4]!.flags & FLAG.SPACER).toBe(FLAG.SPACER);
  expect(frame.cursor).toMatchObject({ x: 5, y: 0, visible: true });
  state.clean();

  // Only the rows that changed come back: the one written to and the
  // one the cursor left.
  term.write(utf8("\r\n\x1b[4mu"));
  frame = state.update(term);
  expect(frame.full).toBe(false);
  expect(frame.lines.map((r) => r.y)).toEqual([0, 1]);
  expect(frame.lines[1]!.cells[0]).toMatchObject({ text: "u", underline: 1 });
  state.clean();
  term.write(utf8("v"));
  frame = state.update(term);
  expect(frame.lines.map((r) => r.y)).toEqual([1]);
  state.clean();
  expect(state.update(term).lines).toEqual([]);
  state.free();
  term.free();
});

test("key names follow KeyboardEvent.code", () => {
  expect(keyName("KeyA")).toBe("A");
  expect(keyName("Digit0")).toBe("DIGIT_0");
  expect(keyName("ArrowUp")).toBe("ARROW_UP");
  expect(keyName("Numpad5")).toBe("NUMPAD_5");
  expect(keyName("F12")).toBe("F12");
  expect(keyName("IntlBackslash")).toBe("INTL_BACKSLASH");
  expect(keyName("MediaTrackNext")).toBe("MEDIA_TRACK_NEXT");
  for (const code of ["KeyA", "Digit0", "ArrowUp", "Backquote", "Enter", "ShiftLeft", "NumpadAdd"]) {
    expect(engine.hasEnumValue("GhosttyKey", keyName(code))).toBe(true);
  }
});

test("keys encode with the terminal's modes", () => {
  const term = engine.terminalNew(10, 2);
  const keys = new KeyEncoder(engine);
  const press = (code: string, key: string, mods: Partial<KeyInput> = {}): string =>
    text(keys.encode(term, { code, key, action: "press", shift: false, ctrl: false, alt: false, meta: false, ...mods }));
  expect(press("KeyA", "a")).toBe("a");
  expect(press("KeyA", "A", { shift: true })).toBe("A");
  expect(press("KeyC", "c", { ctrl: true })).toBe("\x03");
  expect(press("KeyX", "x", { alt: true })).toBe("\x1bx");
  // Option and AltGr compose characters: the text goes through as typed.
  expect(press("Digit8", "[", { alt: true, altComposed: true, unshifted: "8" })).toBe("[");
  expect(press("Digit8", "{", { alt: true, shift: true, altComposed: true, unshifted: "8" })).toBe("{");
  expect(press("Digit2", "@", { ctrl: true, alt: true, altComposed: true, unshifted: "2" })).toBe("@");
  expect(press("KeyB", "∫", { alt: true, altComposed: true, unshifted: "b" })).toBe("∫");
  expect(press("Enter", "Enter")).toBe("\r");
  expect(press("Backspace", "Backspace")).toBe("\x7f");
  expect(press("ArrowUp", "ArrowUp")).toBe("\x1b[A");
  expect(press("ShiftLeft", "Shift")).toBe("");
  expect(press("KeyA", "a", { composing: true })).toBe("");
  term.write(utf8("\x1b[?1h"));
  expect(press("ArrowUp", "ArrowUp")).toBe("\x1bOA");
  // Under the kitty protocol the key is the unshifted character.
  term.write(utf8("\x1b[>11u"));
  expect(press("Digit1", "!", { shift: true, unshifted: "1" })).toBe("\x1b[49;2u");
  expect(press("Digit1", "!", { shift: true, ctrl: true, unshifted: "1" })).toBe("\x1b[49;6u");
  expect(press("KeyA", "A", { shift: true })).toBe("\x1b[97;2u");
  keys.free();
  term.free();
});

test("paste is encoded for the PTY and modes are queried", () => {
  expect(text(engine.encodePaste("a\nb", false))).toBe("a\rb");
  expect(text(engine.encodePaste("a\nb", true))).toBe("\x1b[200~a\nb\x1b[201~");
  const term = engine.terminalNew(10, 2);
  expect(term.mode(2004)).toBe(false);
  term.write(utf8("\x1b[?2004h"));
  expect(term.mode(2004)).toBe(true);
  term.resize(20, 4);
  expect([term.cols, term.rows]).toEqual([20, 4]);
  term.free();
});
