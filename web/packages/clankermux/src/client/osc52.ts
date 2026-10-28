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
const MAX_OSC = 1 << 20;
const PREFIX = "52;";

export interface ClipboardWrite {
  toClipboard: boolean;
  text: string;
}

type State = "text" | "esc" | "maybeOsc52" | "otherOsc";

export class Osc52Scanner {
  private state: State = "text";
  private payload: number[] = [];

  reset(): void {
    this.state = "text";
    this.payload = [];
  }

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
          if (b === 0x5d) this.state = "maybeOsc52";
          else this.state = b === ESC ? "esc" : "text";
          break;
        case "maybeOsc52":
        case "otherOsc":
          if (b === BEL) {
            this.finish(writes);
          } else if (b === ESC) {
            this.finish(writes);
            this.state = "esc";
          } else if (b === CAN || b === SUB) {
            this.reset();
          } else if (this.state === "maybeOsc52") {
            const length = this.payload.length;
            if ((length < PREFIX.length && b !== PREFIX.charCodeAt(length)) || length === MAX_OSC) {
              this.state = "otherOsc";
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
    const collected = this.state === "maybeOsc52";
    const payload = new TextDecoder().decode(Uint8Array.from(this.payload));
    this.reset();
    if (!collected) return;
    const match = /^52;([^;]*);(.*)$/s.exec(payload);
    if (!match || match[2] === "?") return;
    const bytes = base64(match[2]!);
    if (bytes === undefined) return;
    writes.push({ toClipboard: isClipboardSelection(match[1]!), text: new TextDecoder().decode(bytes) });
  }
}

function isClipboardSelection(selection: string): boolean {
  return selection === "" || selection.includes("c");
}

function base64(text: string): Uint8Array | undefined {
  let binary: string;
  try {
    binary = atob(text.replace(/\s+/g, ""));
  } catch {
    return undefined;
  }
  return Uint8Array.from(binary, (c) => c.charCodeAt(0));
}
