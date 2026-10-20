import type { Engine, Terminal } from "./index.ts";

/** A key event as the browser reports it. */
export interface KeyInput {
  /** `KeyboardEvent.code`, the physical key. */
  code: string;
  /** `KeyboardEvent.key`, the text or the key name. */
  key: string;
  action: "press" | "release" | "repeat";
  shift: boolean;
  ctrl: boolean;
  alt: boolean;
  meta: boolean;
  /** An input method is composing; the engine encodes nothing. */
  composing?: boolean;
  /** The character the key produces with no modifiers, from the layout
   * when the browser exposes it. */
  unshifted?: string;
  /** Alt (with Ctrl, for AltGr) composed `key` rather than modifying the
   * key: Option on macOS, AltGr elsewhere. The text is sent as typed. */
  altComposed?: boolean;
}

const MODS = { SHIFT: 1 << 0, CTRL: 1 << 1, ALT: 1 << 2, SUPER: 1 << 3 };

/** `KeyboardEvent.code` as a `GhosttyKey` member name: `KeyA` → `A`, `Digit0` → `DIGIT_0`. */
export function keyName(code: string): string {
  if (/^F\d+$/.test(code)) return code;
  const stripped = code.startsWith("Key") ? code.slice(3) : code;
  return stripped
    .replace(/([a-z])([A-Z0-9])/g, "$1_$2")
    .replace(/([0-9])([A-Z])/g, "$1_$2")
    .toUpperCase();
}

/** Whether `key` is text the key produces rather than a key name such as
 * "Enter" or "ArrowUp" (which, per UI Events, is ASCII letters and digits
 * starting with a capital, two or more of them; text of that shape is one
 * character). */
export function isText(key: string): boolean {
  if (key.length === 0) return false;
  const cp = key.codePointAt(0)!;
  if (cp < 0x20 || cp === 0x7f) return false;
  return key.length === 1 || !/^[A-Z][A-Za-z0-9]+$/.test(key);
}

/** Encodes browser key events with the engine, following the terminal's modes. */
export class KeyEncoder {
  private readonly encoder: number;
  private readonly event: number;
  private readonly unidentified: number;

  constructor(readonly engine: Engine) {
    this.encoder = engine.construct("ghostty_key_encoder_new");
    this.event = engine.construct("ghostty_key_event_new");
    this.unidentified = engine.enumValue("GhosttyKey", "UNIDENTIFIED");
  }

  /** The bytes to send for `input`, empty when the key encodes to nothing. */
  encode(term: Terminal, input: KeyInput): Uint8Array {
    const e = this.engine;
    e.call("ghostty_key_encoder_setopt_from_terminal", this.encoder, term.handle);
    const name = keyName(input.code);
    const key = e.hasEnumValue("GhosttyKey", name) ? e.enumValue("GhosttyKey", name) : this.unidentified;
    // Modifiers that composed the text are not modifiers of the key.
    const composed = input.altComposed === true;
    const mods =
      (input.shift ? MODS.SHIFT : 0) |
      (input.ctrl && !composed ? MODS.CTRL : 0) |
      (input.alt && !composed ? MODS.ALT : 0) |
      (input.meta ? MODS.SUPER : 0);
    e.call("ghostty_key_event_set_action", this.event, e.enumValue("GhosttyKeyAction", input.action.toUpperCase()));
    e.call("ghostty_key_event_set_key", this.event, key);
    e.call("ghostty_key_event_set_mods", this.event, mods);
    const text = isText(input.key) ? input.key : "";
    // Shift that made the text is consumed, as a keyboard reports it, so
    // the kitty protocol's disambiguation sends the text (`:`, `A`) rather
    // than shift and the key. It made it when the text is not the key's
    // own character; with no layout to say, when it is any text but a
    // space.
    const shiftMade = input.shift && text !== "" && (input.unshifted !== undefined ? text !== input.unshifted : text !== " ");
    e.call("ghostty_key_event_set_consumed_mods", this.event, shiftMade ? MODS.SHIFT : 0);
    e.call("ghostty_key_event_set_composing", this.event, input.composing ? 1 : 0);
    const utf8 = new TextEncoder().encode(text);
    return e.withBytes(utf8, (textPtr) => {
      e.call("ghostty_key_event_set_utf8", this.event, textPtr, utf8.length);
      const base = input.unshifted ?? (text ? text.toLowerCase() : "");
      e.call("ghostty_key_event_set_unshifted_codepoint", this.event, base ? base.codePointAt(0)! : 0);
      return e.growing("ghostty_key_encoder_encode", 64, (buf, cap, len) =>
        e.call("ghostty_key_encoder_encode", this.encoder, this.event, buf, cap, len),
      );
    });
  }

  free(): void {
    this.engine.call("ghostty_key_event_free", this.event);
    this.engine.call("ghostty_key_encoder_free", this.encoder);
  }
}
