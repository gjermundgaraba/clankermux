/**
 * Finds OSC 52 clipboard writes in output bytes, across chunk boundaries,
 * ending them as the engine's parser does: BEL or any byte after ESC
 * dispatches the sequence (ESC `\` is the string terminator; ESC `]`
 * begins the next one), CAN and SUB abandon it. The engine raises the
 * clipboard effect through a callback, which from wasm would need a JS
 * function in the module's function table (a generated wrapper per
 * signature); the scan is less machinery. OSC 1337 and 5522 clipboard
 * forms are not scanned for, and an OSC 52 inside another string (a DCS
 * passthrough) is taken as one, where the engine would not.
 */
const ESC = 0x1b;
const BEL = 0x07;
const CAN = 0x18;
const SUB = 0x1a;
/** Longer OSC 52 payloads are abandoned, not buffered. */
const MAX_OSC = 1 << 20;
const PREFIX = "52;";

export interface ClipboardWrite {
  /** The selection parameter: `c` or empty for the clipboard, `p`, `s`, `q` otherwise. */
  selection: string;
  text: string;
}

/** `osc`: inside an OSC that may still be a 52, collecting its payload;
 * `skip`: inside any other OSC, skipped to its end. */
type State = "text" | "esc" | "osc" | "skip";

export class Osc52Scanner {
  private state: State = "text";
  private payload: number[] = [];

  /** Forgets a sequence in progress: the stream it came from is gone. */
  reset(): void {
    this.state = "text";
    this.payload = [];
  }

  /** Feeds output bytes; returns the clipboard writes they completed. */
  feed(bytes: Uint8Array): ClipboardWrite[] {
    const writes: ClipboardWrite[] = [];
    for (let at = 0; at < bytes.length; at++) {
      const b = bytes[at]!;
      switch (this.state) {
        case "text":
          at = bytes.indexOf(ESC, at);
          if (at < 0) return writes;
          this.state = "esc";
          break;
        case "esc":
          // The payload is empty: every way out of an OSC clears it.
          if (b === 0x5d) this.state = "osc";
          else this.state = b === ESC ? "esc" : "text";
          break;
        case "osc":
        case "skip":
          if (b === BEL) {
            this.finish(writes);
          } else if (b === ESC) {
            this.finish(writes);
            this.state = "esc";
          } else if (b === CAN || b === SUB) {
            this.reset();
          } else if (this.state === "osc") {
            const length = this.payload.length;
            if ((length < PREFIX.length && b !== PREFIX.charCodeAt(length)) || length === MAX_OSC) {
              this.state = "skip";
              this.payload = [];
            } else {
              this.payload.push(b);
            }
          }
          break;
      }
    }
    return writes;
  }

  private finish(writes: ClipboardWrite[]): void {
    const collected = this.state === "osc";
    const payload = new TextDecoder().decode(Uint8Array.from(this.payload));
    this.reset();
    if (!collected) return;
    const match = /^52;([^;]*);(.*)$/s.exec(payload);
    if (!match || match[2] === "?") return;
    try {
      writes.push({ selection: match[1]!, text: new TextDecoder().decode(base64(match[2]!)) });
    } catch {
      // Not base64: ignored, as a terminal would.
    }
  }
}

function base64(text: string): Uint8Array {
  const binary = atob(text.replace(/\s+/g, ""));
  return Uint8Array.from(binary, (c) => c.charCodeAt(0));
}
