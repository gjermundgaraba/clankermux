import { expect, test } from "vitest";

import { type KeyEventLike, type Keyboard, Typing, browserKeeps, keyInput } from "../../src/client/keyboard.ts";

const event = (extra: Partial<KeyEventLike>): KeyEventLike => ({
  type: "keydown",
  code: "KeyA",
  key: "a",
  repeat: false,
  shiftKey: false,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  isComposing: false,
  keyCode: 0,
  ...extra,
});
const layout: Keyboard = { layout: new Map([["KeyA", "a"], ["Digit8", "8"], ["KeyB", "b"], ["Digit2", "2"]]), mac: true };
const blind = (mac: boolean): Keyboard => ({ layout: undefined, mac });

test("events map to engine key input", () => {
  expect(keyInput(event({}), layout)).toEqual({
    code: "KeyA",
    key: "a",
    action: "press",
    shift: false,
    ctrl: false,
    alt: false,
    meta: false,
    unshifted: "a",
  });
  expect(keyInput(event({ repeat: true, ctrlKey: true }), layout).action).toBe("repeat");
  expect(keyInput(event({ type: "keyup" }), layout).action).toBe("release");
  expect(keyInput(event({ isComposing: true }), layout).composing).toBe(true);
  expect("unshifted" in keyInput(event({}), blind(true))).toBe(false);
});

test("alt that composed a character is not a modifier", () => {
  // Option+8 on a German Mac, AltGr+2 on Windows, Option+b on a US Mac.
  expect(keyInput(event({ code: "Digit8", key: "[", altKey: true }), layout).altComposed).toBe(true);
  expect(keyInput(event({ code: "Digit2", key: "@", altKey: true, ctrlKey: true }), layout).altComposed).toBe(true);
  expect(keyInput(event({ code: "KeyB", key: "\u222b", altKey: true }), layout).altComposed).toBe(true);
  // Linux Alt+x and Ctrl+Alt+x: the key's own character, a real modifier.
  const linux: Keyboard = { ...layout, mac: false };
  expect(keyInput(event({ code: "KeyA", key: "a", altKey: true }), linux).altComposed).toBeUndefined();
  expect(keyInput(event({ code: "KeyA", key: "a", altKey: true, ctrlKey: true }), linux).altComposed).toBeUndefined();
  // Without a layout the platform decides.
  expect(keyInput(event({ key: "a", altKey: true }), blind(true)).altComposed).toBe(true);
  expect(keyInput(event({ key: "a", altKey: true }), blind(false)).altComposed).toBeUndefined();
  // A key name is never composed text.
  expect(keyInput(event({ code: "ArrowUp", key: "ArrowUp", altKey: true }), blind(true)).altComposed).toBeUndefined();
});

test("command-key combinations and Ctrl+Shift+V stay with the browser", () => {
  expect(browserKeeps(event({ metaKey: true, code: "KeyV", key: "v" }))).toBe(true);
  expect(browserKeeps(event({ ctrlKey: true, code: "KeyC", key: "c" }))).toBe(false);
  expect(browserKeeps(event({ ctrlKey: true, code: "KeyV", key: "v" }))).toBe(false);
  expect(browserKeeps(event({ ctrlKey: true, shiftKey: true, code: "KeyV", key: "V" }))).toBe(true);
});

test("typing sends keys, and a key's release only after its press", () => {
  const typing = new Typing(layout);
  expect(typing.keydown(event({}))?.action).toBe("press");
  expect(typing.keyup(event({ type: "keyup" }))?.action).toBe("release");
  expect(typing.keyup(event({ type: "keyup" }))).toBeUndefined();
  // The browser's own: Cmd+V pastes, and its release is not sent either.
  expect(typing.keydown(event({ code: "KeyV", key: "v", metaKey: true }))).toBeUndefined();
  expect(typing.keyup(event({ type: "keyup", code: "KeyV", key: "v" }))).toBeUndefined();
});

test("a dead key composes: its keys stay with the browser and the character is sent as text", () => {
  // Option+E, then E, on a US Mac in Chrome: é.
  const typing = new Typing(layout);
  expect(typing.keydown(event({ code: "AltLeft", key: "Alt", altKey: true }))?.code).toBe("AltLeft");
  expect(typing.keydown(event({ code: "KeyE", key: "Dead", altKey: true }))).toBeUndefined();
  expect(typing.beforeinput({ inputType: "insertCompositionText", data: "\u00b4", isComposing: true })).toBeUndefined();
  expect(typing.keyup(event({ type: "keyup", code: "KeyE", key: "\u00b4", altKey: true }))).toBeUndefined();
  expect(typing.keyup(event({ type: "keyup", code: "AltLeft", key: "Alt" }))?.action).toBe("release");
  expect(typing.keydown(event({ code: "KeyE", key: "\u00e9", isComposing: true }))).toBeUndefined();
  expect(typing.beforeinput({ inputType: "insertCompositionText", data: "\u00e9", isComposing: true })).toBeUndefined();
  expect(typing.compositionend("\u00e9")).toEqual({ text: "\u00e9" });
  expect(typing.keyup(event({ type: "keyup", code: "KeyE", key: "e" }))).toBeUndefined();
});

test("an input method's composition is sent once, when it ends", () => {
  // Japanese romaji: n, i, h, o, n, then Enter commits 日本.
  const typing = new Typing(layout);
  for (const code of ["KeyN", "KeyI", "KeyH", "KeyO", "KeyN"]) {
    expect(typing.keydown(event({ code, key: "Process", keyCode: 229 }))).toBeUndefined();
    expect(typing.beforeinput({ inputType: "insertCompositionText", data: "\u306b", isComposing: true })).toBeUndefined();
    expect(typing.keyup(event({ type: "keyup", code, key: code.slice(3).toLowerCase(), isComposing: true }))).toBeUndefined();
  }
  // Backspace inside the composition is the input method's.
  expect(typing.keydown(event({ code: "Backspace", key: "Backspace", isComposing: true, keyCode: 229 }))).toBeUndefined();
  expect(typing.beforeinput({ inputType: "deleteContentBackward", data: null, isComposing: true })).toBeUndefined();
  expect(typing.keydown(event({ code: "Enter", key: "Enter", isComposing: true, keyCode: 229 }))).toBeUndefined();
  expect(typing.compositionend("\u65e5\u672c")).toEqual({ text: "\u65e5\u672c" });
  // The Enter that committed it sends no release, and nothing else.
  expect(typing.keyup(event({ type: "keyup", code: "Enter", key: "Enter" }))).toBeUndefined();
  // A composition cancelled to nothing sends nothing.
  expect(typing.compositionend("")).toBeUndefined();
});

test("a key the input method processes is its own, with key code 229 or without", () => {
  const typing = new Typing(layout);
  expect(typing.keydown(event({ key: "Process" }))).toBeUndefined();
  expect(typing.keydown(event({ keyCode: 229 }))).toBeUndefined();
});

test("text and edits that no key sent reach the session", () => {
  const typing = new Typing(layout);
  // The emoji picker, dictation, a virtual keyboard.
  expect(typing.beforeinput({ inputType: "insertText", data: "\u{1f389}", isComposing: false })).toEqual({ text: "\u{1f389}" });
  expect(typing.beforeinput({ inputType: "deleteContentBackward", data: null, isComposing: false })).toEqual({
    key: { code: "Backspace", key: "Backspace", action: "press", shift: false, ctrl: false, alt: false, meta: false },
  });
  expect(typing.beforeinput({ inputType: "insertLineBreak", data: null, isComposing: false })).toMatchObject({ key: { code: "Enter" } });
  // A paste is the paste event's.
  expect(typing.beforeinput({ inputType: "insertFromPaste", data: null, isComposing: false })).toBeUndefined();
});
