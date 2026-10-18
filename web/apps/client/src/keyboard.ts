import type { KeyInput } from "@clankermux/engine";

/** The parts of a `KeyboardEvent` the mapping reads. */
export interface KeyEventLike {
  type: string;
  code: string;
  key: string;
  repeat: boolean;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  isComposing: boolean;
}

/** What the page knows about the keyboard: the layout's unmodified
 * character per `code` when the browser exposes it, and the platform. */
export interface Keyboard {
  layout: ReadonlyMap<string, string> | undefined;
  mac: boolean;
}

/** Whether the browser keeps the key: its own shortcuts with the platform's
 * command modifier, and Ctrl+Shift+V, the paste shortcut where Ctrl+V is
 * the terminal's. */
export function browserKeeps(e: KeyEventLike): boolean {
  return e.metaKey || (e.ctrlKey && e.shiftKey && e.code === "KeyV");
}

/** Whether `key` is one character of text rather than a key name. */
function isText(key: string): boolean {
  if (key.length === 0) return false;
  const cp = key.codePointAt(0)!;
  return cp >= 0x20 && cp !== 0x7f && [...key].length === 1;
}

export function keyInput(e: KeyEventLike, keyboard: Keyboard): KeyInput {
  const input: KeyInput = {
    code: e.code,
    key: e.key,
    action: e.type === "keyup" ? "release" : e.repeat ? "repeat" : "press",
    shift: e.shiftKey,
    ctrl: e.ctrlKey,
    alt: e.altKey,
    meta: e.metaKey,
  };
  if (e.isComposing) input.composing = true;
  const unshifted = keyboard.layout?.get(e.code);
  if (unshifted !== undefined) input.unshifted = unshifted;
  // Alt composed the text when it differs from the key's own character
  // (Option on macOS, AltGr elsewhere). Without a layout to compare
  // against, macOS Option composes and other platforms' Alt modifies.
  if (e.altKey && isText(e.key)) {
    const composed = unshifted !== undefined ? e.key.toLowerCase() !== unshifted.toLowerCase() : keyboard.mac;
    if (composed) input.altComposed = true;
  }
  return input;
}
