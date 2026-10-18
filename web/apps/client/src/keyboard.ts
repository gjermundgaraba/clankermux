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

/** Whether the browser keeps the key: its own shortcuts, taken with the
 * platform's command modifier. */
export function browserKeeps(e: KeyEventLike): boolean {
  return e.metaKey;
}

export function keyInput(e: KeyEventLike): KeyInput {
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
  return input;
}
