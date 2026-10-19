import { type KeyInput, isText } from "../engine/index.ts";

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
  /** 229 while an input method processes the key. */
  keyCode: number;
}

/** The parts of an `InputEvent` the input element reads. */
export interface InputEventLike {
  inputType: string;
  data: string | null;
  isComposing: boolean;
}

/** What typing comes to: a key for the key encoder, or text as typed. */
export type Typed = { key: KeyInput } | { text: string };

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

/** Whether the key belongs to an input method: it is composing, or
 * processing the key (`Process`, key code 229), or the key is a dead key
 * that starts a composition. Its text arrives at `compositionend`. */
export function composing(e: KeyEventLike): boolean {
  return e.isComposing || e.keyCode === 229 || e.key === "Process" || e.key === "Dead";
}

/** A key with no modifiers, pressed. */
function pressed(code: string): KeyInput {
  return { code, key: code, action: "press", shift: false, ctrl: false, alt: false, meta: false };
}

/**
 * Typing on the terminal's input element, a textarea: keys go to the key
 * encoder, and text that no key sent goes as text. Keys the browser keeps
 * and keys of a composition are left to the browser; a composition's text
 * is sent when it ends, and text inserted without a key (a dead key's
 * character, the emoji picker, dictation) as it is inserted.
 */
export class Typing {
  /** Keys whose press was sent, so their release is too: a key that
   * ended a composition sends no release of a press it never sent. */
  private readonly down = new Set<string>();

  constructor(private readonly keyboard: Keyboard) {}

  /** A key to send, or undefined to leave the event to the browser. */
  keydown(e: KeyEventLike): KeyInput | undefined {
    if (browserKeeps(e) || composing(e)) return undefined;
    this.down.add(e.code);
    return keyInput(e, this.keyboard);
  }

  keyup(e: KeyEventLike): KeyInput | undefined {
    if (!this.down.delete(e.code)) return undefined;
    return keyInput(e, this.keyboard);
  }

  /** `beforeinput`: what the element would insert or delete, when no key
   * sent it. A composition's own steps wait for `compositionend`; a paste
   * is the `paste` event's. */
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

  /** `compositionend`: the composed text. */
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
  // Alt composed the text when it differs from the key's own character
  // (Option on macOS, AltGr elsewhere). Without a layout to compare
  // against, macOS Option composes and other platforms' Alt modifies.
  if (e.altKey && isText(e.key)) {
    const composed = unshifted !== undefined ? e.key.toLowerCase() !== unshifted.toLowerCase() : keyboard.mac;
    if (composed) input.altComposed = true;
  }
  return input;
}
