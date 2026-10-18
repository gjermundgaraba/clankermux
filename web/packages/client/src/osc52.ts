/**
 * Finds OSC 52 clipboard writes in output bytes, across chunk boundaries.
 * The engine raises the clipboard effect through a callback, which the
 * wasm build cannot reach, so the client scans the stream itself.
 */
const ESC = 0x1b;
const BEL = 0x07;
/** Longer OSC payloads are abandoned, not buffered. */
const MAX_OSC = 1 << 20;

export interface ClipboardWrite {
  /** The selection parameter, `c` for the clipboard. */
  selection: string;
  text: string;
}

export class Osc52Scanner {
  private state: "text" | "esc" | "osc" | "osc-esc" = "text";
  private payload: number[] = [];
  private overflowed = false;

  /** Feeds output bytes; returns the clipboard writes they completed. */
  feed(bytes: Uint8Array): ClipboardWrite[] {
    const writes: ClipboardWrite[] = [];
    for (const b of bytes) {
      switch (this.state) {
        case "text":
          if (b === ESC) this.state = "esc";
          break;
        case "esc":
          if (b === 0x5d) {
            this.state = "osc";
            this.payload = [];
            this.overflowed = false;
          } else {
            this.state = b === ESC ? "esc" : "text";
          }
          break;
        case "osc":
          if (b === BEL) {
            this.finish(writes);
          } else if (b === ESC) {
            this.state = "osc-esc";
          } else if (this.payload.length < MAX_OSC) {
            this.payload.push(b);
          } else {
            this.overflowed = true;
          }
          break;
        case "osc-esc":
          if (b === 0x5c) {
            this.finish(writes);
          } else {
            // Not ST: the OSC was cut short by another sequence.
            this.state = b === ESC ? "esc" : "text";
          }
          break;
      }
    }
    return writes;
  }

  private finish(writes: ClipboardWrite[]): void {
    this.state = "text";
    if (this.overflowed) return;
    const payload = new TextDecoder().decode(Uint8Array.from(this.payload));
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
