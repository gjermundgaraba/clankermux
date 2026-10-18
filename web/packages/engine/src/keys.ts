import { type Engine, EngineError, RESULT, type Terminal } from "./index.ts";

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

/** Whether `key` is text the key produces rather than a key name. */
function isText(key: string): boolean {
  if (key.length === 0) return false;
  const cp = key.codePointAt(0)!;
  if (cp < 0x20 || cp === 0x7f) return false;
  // A key name such as "Enter" or "ArrowUp" has several code points; text
  // has one (or one grapheme such as an emoji).
  return [...key].length === 1;
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
    const mods =
      (input.shift ? MODS.SHIFT : 0) |
      (input.ctrl ? MODS.CTRL : 0) |
      (input.alt ? MODS.ALT : 0) |
      (input.meta ? MODS.SUPER : 0);
    e.call("ghostty_key_event_set_action", this.event, e.enumValue("GhosttyKeyAction", input.action.toUpperCase()));
    e.call("ghostty_key_event_set_key", this.event, key);
    e.call("ghostty_key_event_set_mods", this.event, mods);
    e.call("ghostty_key_event_set_consumed_mods", this.event, 0);
    e.call("ghostty_key_event_set_composing", this.event, input.composing ? 1 : 0);
    const text = isText(input.key) ? input.key : "";
    const utf8 = new TextEncoder().encode(text);
    const textPtr = utf8.length ? e.copyIn(utf8) : 0;
    try {
      e.call("ghostty_key_event_set_utf8", this.event, textPtr, utf8.length);
      const unshifted = text ? text.toLowerCase().codePointAt(0)! : 0;
      e.call("ghostty_key_event_set_unshifted_codepoint", this.event, unshifted);
      let cap = 64;
      for (;;) {
        const out = e.scratch(cap, (buf) =>
          e.scratch(4, (len) => {
            const code = e.call("ghostty_key_encoder_encode", this.encoder, this.event, buf, cap, len);
            const n = e.view.getUint32(len, true);
            if (code === RESULT.OUT_OF_SPACE) return n;
            if (code !== RESULT.SUCCESS) throw new EngineError("ghostty_key_encoder_encode", code);
            return e.memory.slice(buf, buf + n);
          }),
        );
        if (typeof out !== "number") return out;
        cap = out;
      }
    } finally {
      if (textPtr) e.free(textPtr, utf8.length);
    }
  }

  free(): void {
    this.engine.call("ghostty_key_event_free", this.event);
    this.engine.call("ghostty_key_encoder_free", this.encoder);
  }
}
