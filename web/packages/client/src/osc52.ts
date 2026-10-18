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

type State = "text" | "esc" | "osc";

export class Osc52Scanner {
  private state: State = "text";
  /** Inside an OSC that may still be a 52; otherwise its bytes are skipped. */
  private collecting = false;
  private payload: number[] = [];

  /** Forgets a sequence in progress: the stream it came from is gone. */
  reset(): void {
    this.state = "text";
    this.collecting = false;
    this.payload = [];
  }

  private startOsc(): void {
    this.state = "osc";
    this.collecting = true;
    this.payload = [];
  }

  /** Feeds output bytes; returns the clipboard writes they completed. */
  feed(bytes: Uint8Array): ClipboardWrite[] {
    const writes: ClipboardWrite[] = [];
    for (const b of bytes) {
      switch (this.state) {
        case "text":
          if (b === ESC) this.state = "esc";
          break;
        case "esc":
          if (b === 0x5d) this.startOsc();
          else this.state = b === ESC ? "esc" : "text";
          break;
        case "osc":
          if (b === BEL) {
            this.finish(writes);
          } else if (b === ESC) {
            this.finish(writes);
            this.state = "esc";
          } else if (b === CAN || b === SUB) {
            this.reset();
          } else if (this.collecting) {
            this.payload.push(b);
            // Only a 52 is collected; anything else is skipped to its end.
            const head = String.fromCharCode(...this.payload.slice(0, PREFIX.length));
            if (!PREFIX.startsWith(head) || this.payload.length > MAX_OSC) {
              this.collecting = false;
              this.payload = [];
            }
          }
          break;
      }
    }
    return writes;
  }

  private finish(writes: ClipboardWrite[]): void {
    const collected = this.collecting;
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
