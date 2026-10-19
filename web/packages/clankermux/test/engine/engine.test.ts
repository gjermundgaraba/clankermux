import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { beforeAll, expect, test } from "vitest";

import { Decoder, Engine, KeyEncoder, type KeyInput, RenderState, SelectionGesture, keyName } from "../../src/engine/index.ts";
import { readyEnd, stepEnd } from "../../src/engine/decoder.ts";
import { FLAG, NO_COLOR } from "../../src/engine/render.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
let engine: Engine;
beforeAll(async () => {
  engine = await Engine.load(readFileSync(`${here}../../ghostty-vt.wasm`));
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

test("the viewport scrolls through the history and stays there while output arrives", () => {
  const term = engine.terminalNew(10, 3);
  term.write(utf8("l1\r\nl2\r\nl3\r\nl4\r\nl5\r\nl6"));
  const render = new RenderState(engine);
  const rows = (frame: ReturnType<RenderState["update"]>) =>
    frame.lines.map((l) => l.cells.map((c) => c.text || " ").join("").trimEnd());
  expect(render.update(term).scrollbar).toEqual({ total: 6, offset: 3, len: 3 });
  render.clean();

  // A move is a full frame; the cursor, below the view, is not in it.
  term.scroll(-2);
  let frame = render.update(term);
  render.clean();
  expect(frame.full).toBe(true);
  expect(rows(frame)).toEqual(["l2", "l3", "l4"]);
  expect(frame.cursor).toBeUndefined();
  expect(frame.scrollbar).toEqual({ total: 6, offset: 1, len: 3 });

  // Output adds history below the view and changes no row of it.
  term.write(utf8("\r\nl7\r\nl8"));
  frame = render.update(term);
  render.clean();
  expect(frame.lines).toEqual([]);
  expect(frame.scrollbar).toEqual({ total: 8, offset: 1, len: 3 });

  term.scroll("top");
  expect(term.scrollbar.offset).toBe(0);
  term.scroll("bottom");
  frame = render.update(term);
  render.clean();
  expect(rows(frame)).toEqual(["l6", "l7", "l8"]);
  expect(frame.scrollbar).toEqual({ total: 8, offset: 5, len: 3 });

  // The alternate screen has no history to scroll.
  expect(term.alternate).toBe(false);
  term.write(utf8("\x1b[?1049h"));
  expect(term.alternate).toBe(true);
  expect(term.scrollbar).toEqual({ total: 3, offset: 0, len: 3 });
  term.scroll(-2);
  expect(term.scrollbar.offset).toBe(0);
  render.free();
  term.free();
});

test("a selection drags by cells, words and lines, copies wrapped lines whole, and follows output", () => {
  const term = engine.terminalNew(10, 3);
  // "three four five" wraps after ten columns.
  term.write(utf8("one two\r\nthree four five"));
  const gesture = new SelectionGesture(engine);
  const render = new RenderState(engine);
  render.update(term);
  render.clean();

  // A press selects nothing; a drag selects from the pressed cell to the
  // pointer's, the half of a cell it passed deciding the last.
  expect(gesture.press(term, { x: 0.2, y: 0.5 }, 1000)).toBeUndefined();
  expect(gesture.drag(term, { x: 0.4, y: 0.5 })).toBeUndefined();
  const drag = gesture.drag(term, { x: 2.6, y: 2.5 });
  expect(drag).toBeDefined();
  term.select(drag);
  gesture.release(term, { x: 2.6, y: 2.5 });
  expect(term.selected).toBe(true);
  // The wrapped line comes back whole.
  expect(term.selectionText()).toBe("one two\nthree four fi");
  const frame = render.update(term);
  render.clean();
  expect(frame.lines.map((l) => l.selection)).toEqual([
    [0, 9],
    [0, 9],
    [0, 2],
  ]);

  // Output scrolls the selected text up a row; the selection goes with it.
  term.write(utf8("\r\nsix"));
  expect(term.selectionText()).toBe("one two\nthree four fi");
  expect(render.update(term).lines.map((l) => l.selection)).toEqual([[0, 9], [0, 2], undefined]);
  render.clean();

  // Two presses select a word, three a line (the wrapped one whole).
  const at = { x: 1.5, y: 0.5 };
  gesture.press(term, at, 5000);
  gesture.release(term, at);
  term.select(gesture.press(term, at, 5100));
  expect(term.selectionText()).toBe("three");
  gesture.release(term, at);
  term.select(gesture.press(term, at, 5200));
  expect(term.selectionText()).toBe("three four five");
  gesture.release(term, at);

  term.select(undefined);
  expect(term.selected).toBe(false);
  expect(term.selectionText()).toBeUndefined();
  expect(render.update(term).lines.every((l) => l.selection === undefined)).toBe(true);
  gesture.free();
  render.free();
  term.free();
});
