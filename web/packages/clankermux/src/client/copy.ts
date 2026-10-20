/** The page's copies to the clipboard, and what each came to. */

/** What started a copy: the release of a selection, Ctrl+Shift+C, the
 * browser's copy (Cmd+C, the Copy menu), or the program (OSC 52). */
export type CopySource = "release" | "keys" | "browser" | "program";

/** Why a copy failed: the browser refused the write (`message` says
 * what it gave), it did not finish within two seconds, or the page has no
 * clipboard (outside a secure context). */
export type CopyFailure = "refused" | "timeout" | "unavailable";

/** A copy's outcome. `chars` is the text's length in characters (code
 * points); a copy that timed out before the text was known has none. */
export type Copy =
  | { ok: true; source: CopySource; chars: number }
  | { ok: false; source: CopySource; reason: CopyFailure; chars?: number; message?: string };

/** The parts of `navigator.clipboard` a copy uses. */
export interface ClipboardLike {
  write?(items: ClipboardItem[]): Promise<void>;
  writeText?(text: string): Promise<void>;
}

export interface CopyHost {
  /** The clipboard, read at each copy; undefined where there is none. */
  clipboard(): ClipboardLike | undefined;
  /** A `ClipboardItem` of text to come; undefined where the browser has
   * no `ClipboardItem`. */
  item: ((text: Promise<Blob>) => ClipboardItem) | undefined;
  report(copy: Copy): void;
}

/** How long a copy may take: the worker's answer and the write. */
export const COPY_TIMEOUT_MS = 2000;

/** Characters by code point, as a page counts them for display. */
const length = (text: string) => [...text].length;

/** One copy under way, reported once: when both its text's length and
 * the write's result are known, or at the timeout. */
class Attempt {
  private done = false;
  private chars: number | undefined;
  private result: { ok: true } | { ok: false; reason: CopyFailure; message?: string } | undefined;
  private readonly timer: ReturnType<typeof setTimeout>;

  constructor(
    private readonly source: CopySource,
    private readonly report: (copy: Copy) => void,
    timeoutMs: number,
    /** At the timeout, after the report. */
    timedOut?: () => void,
  ) {
    this.timer = setTimeout(() => {
      this.finish({ ok: false, reason: "timeout" });
      timedOut?.();
    }, timeoutMs);
  }

  get settled(): boolean {
    return this.done;
  }

  text(text: string): void {
    this.chars = length(text);
    this.settle();
  }

  wrote(result: { ok: true } | { ok: false; reason: CopyFailure; message?: string }): void {
    this.result ??= result;
    this.settle();
  }

  /** Not a copy: nothing was selected, or a newer copy took its place. */
  drop(): void {
    this.done = true;
    clearTimeout(this.timer);
  }

  private settle(): void {
    if (this.chars !== undefined && this.result) this.finish(this.result);
  }

  private finish(result: { ok: true } | { ok: false; reason: CopyFailure; message?: string }): void {
    if (this.done) return;
    this.drop();
    const chars = this.chars;
    if (result.ok) this.report({ ok: true, source: this.source, chars: chars! });
    else this.report({ source: this.source, ...result, ...(chars !== undefined ? { chars } : {}) });
  }

  /** Follows a write's promise. */
  follow(write: Promise<void>): void {
    write.then(
      () => this.wrote({ ok: true }),
      (err: unknown) => this.wrote({ ok: false, reason: "refused", message: String(err) }),
    );
  }
}

/** A release's copy, waiting for the worker to say what was selected. */
interface Pending {
  attempt: Attempt;
  /** The text for the write under way; none where it waits to start. */
  text?: { resolve: (text: string) => void; reject: () => void };
}

/** A mounted terminal's copies. */
export class Copier {
  /** The releases' copies in the order the worker answers them. */
  private readonly pending: Pending[] = [];

  constructor(
    private readonly host: CopyHost,
    private readonly timeoutMs = COPY_TIMEOUT_MS,
  ) {}

  /** Starts the copy of what a release selected. Safari writes to the
   * clipboard only within the user's gesture, which is over before the
   * worker answers: the write starts here with the text to come, and
   * fails, leaving the clipboard alone, when nothing was selected. Where
   * the browser has no `ClipboardItem` the text is written once it
   * arrives. An earlier release's copy still waiting is dropped, so the
   * clipboard gets the latest. */
  start(): void {
    for (const earlier of this.pending) this.dropOne(earlier);
    const pending: Pending = { attempt: new Attempt("release", this.host.report, this.timeoutMs, () => pending.text?.reject()) };
    this.pending.push(pending);
    const clipboard = this.host.clipboard();
    if (!this.host.item || !clipboard?.write) return;
    const text = new Promise<Blob>((resolve, reject) => {
      pending.text = {
        resolve: (value) => resolve(new Blob([value], { type: "text/plain" })),
        reject: () => reject(new Error("nothing selected")),
      };
    });
    // A browser that refuses the write never reads `text`, whose
    // rejection would then go unhandled.
    text.catch(() => {});
    let item: ClipboardItem;
    try {
      item = this.host.item(text);
    } catch {
      // One whose `ClipboardItem` takes no text to come: written once
      // the text arrives.
      delete pending.text;
      return;
    }
    pending.attempt.follow(clipboard.write([item]));
  }

  /** The worker's answer to the earliest release waiting: the selected
   * text, or undefined when nothing was selected, which is no copy. */
  answer(text: string | undefined): void {
    const pending = this.pending.shift();
    if (!pending) return;
    if (text === undefined || pending.attempt.settled) return this.dropOne(pending);
    pending.attempt.text(text);
    if (pending.text) pending.text.resolve(text);
    else this.writeText(pending.attempt, text);
  }

  /** Copies `text` known now. */
  write(text: string, source: CopySource): void {
    const attempt = new Attempt(source, this.host.report, this.timeoutMs);
    attempt.text(text);
    this.writeText(attempt, text);
  }

  /** A copy the browser made itself (the `copy` event's data). */
  copied(text: string, source: CopySource): void {
    this.host.report({ ok: true, source, chars: length(text) });
  }

  /** Drops the copies waiting for their text, unreported. */
  dispose(): void {
    for (const pending of this.pending.splice(0)) this.dropOne(pending);
  }

  private dropOne(pending: Pending): void {
    pending.attempt.drop();
    pending.text?.reject();
  }

  private writeText(attempt: Attempt, text: string): void {
    const clipboard = this.host.clipboard();
    if (!clipboard?.writeText) return attempt.wrote({ ok: false, reason: "unavailable" });
    attempt.follow(clipboard.writeText(text));
  }
}
