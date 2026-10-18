import { expect, test } from "vitest";

import { type KeyEventLike, browserKeeps, keyInput } from "../src/keyboard.ts";

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

test("events map to engine key input", () => {
  expect(keyInput(event({}))).toEqual({
    code: "KeyA",
    key: "a",
    action: "press",
    shift: false,
    ctrl: false,
    alt: false,
    meta: false,
  });
  expect(keyInput(event({ repeat: true, ctrlKey: true })).action).toBe("repeat");
  expect(keyInput(event({ type: "keyup" })).action).toBe("release");
  expect(keyInput(event({ isComposing: true })).composing).toBe(true);
  expect("composing" in keyInput(event({}))).toBe(false);
});

test("command-key combinations stay with the browser", () => {
  expect(browserKeeps(event({ metaKey: true, code: "KeyV", key: "v" }))).toBe(true);
  expect(browserKeeps(event({ ctrlKey: true, code: "KeyC", key: "c" }))).toBe(false);
});
