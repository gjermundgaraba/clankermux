import { type KeyInput, isText } from "../engine/index.ts";

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
  keyCode: number;
}

export interface InputEventLike {
  inputType: string;
  data: string | null;
  isComposing: boolean;
}

export type Typed = { key: KeyInput } | { text: string };

export interface Keyboard {
  layout: ReadonlyMap<string, string> | undefined;
  mac: boolean;
}

const IME_PROCESSING_KEY_CODE = 229;

function isTerminalPasteShortcut(e: KeyEventLike): boolean {
  return e.ctrlKey && e.shiftKey && e.code === "KeyV";
}

export function browserKeeps(e: KeyEventLike): boolean {
  return e.metaKey || isTerminalPasteShortcut(e);
}

export function composing(e: KeyEventLike): boolean {
  return e.isComposing || e.keyCode === IME_PROCESSING_KEY_CODE || e.key === "Process" || e.key === "Dead";
}

function pressed(code: string): KeyInput {
  return { code, key: code, action: "press", shift: false, ctrl: false, alt: false, meta: false };
}

export class Typing {
  private readonly pressesSent = new Set<string>();

  constructor(private readonly keyboard: Keyboard) {}

  keydown(e: KeyEventLike): KeyInput | undefined {
    if (browserKeeps(e) || composing(e)) return undefined;
    this.pressesSent.add(e.code);
    return keyInput(e, this.keyboard);
  }

  keyup(e: KeyEventLike): KeyInput | undefined {
    if (!this.pressesSent.delete(e.code)) return undefined;
    return keyInput(e, this.keyboard);
  }

  beforeinput(e: InputEventLike): Typed | undefined {
    if (e.isComposing) return undefined;
    switch (e.inputType) {
      case "insertText":
        return e.data ? { text: e.data } : undefined;
      case "insertLineBreak":
      case "insertParagraph":
        return { key: pressed("Enter") };
      case "deleteContentBackward":
        return { key: pressed("Backspace") };
      default:
        return undefined;
    }
  }

  compositionend(data: string | null): Typed | undefined {
    return data ? { text: data } : undefined;
  }
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
  if (altComposedText(e, unshifted, keyboard)) input.altComposed = true;
  return input;
}

function altComposedText(e: KeyEventLike, unshifted: string | undefined, keyboard: Keyboard): boolean {
  if (!e.altKey || !isText(e.key)) return false;
  if (unshifted === undefined) return keyboard.mac;
  return e.key.toLowerCase() !== unshifted.toLowerCase();
}
