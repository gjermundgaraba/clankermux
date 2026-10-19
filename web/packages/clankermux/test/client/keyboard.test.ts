import { expect, test } from "vitest";

import { type KeyEventLike, type Keyboard, browserKeeps, keyInput } from "../../src/client/keyboard.ts";

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
