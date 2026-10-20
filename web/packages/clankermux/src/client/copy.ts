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
const COPY_TIMEOUT_MS = 2000;

/** How a clipboard write went. */
type Written = { ok: true } | { ok: false; reason: CopyFailure; message?: string };

/** Characters by code point, as a page counts them for display. */
const length = (text: string) => [...text].length;

/** One copy under way, reported once: when both its text's length and
 * the write's result are known, or at the timeout. */
class Attempt {
  private done = false;
  private chars: number | undefined;
  private result: Written | undefined;
  private readonly timer: ReturnType<typeof setTimeout>;

  constructor(
    private readonly source: CopySource,
    private readonly report: (copy: Copy) => void,
    /** Once, when it is reported or dropped. */
    private readonly ended: () => void,
    /** At the timeout, after the report. */
    timedOut?: () => void,
  ) {
    this.timer = setTimeout(() => {
      this.finish({ ok: false, reason: "timeout" });
      timedOut?.();
    }, COPY_TIMEOUT_MS);
  }

  text(text: string): void {
    this.chars = length(text);
    this.settle();
  }

  wrote(result: Written): void {
    this.result ??= result;
    this.settle();
  }

  /** Not a copy: nothing was selected, or a newer copy took its place. */
  drop(): void {
    if (this.done) return;
    this.done = true;
    clearTimeout(this.timer);
    this.ended();
  }

  private settle(): void {
    if (this.chars !== undefined && this.result) this.finish(this.result);
  }

  private finish(result: Written): void {
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
  /** The releases' copies waiting for the worker's answer, by number. */
  private readonly pending = new Map<number, Pending>();
  private next = 1;
  /** Every copy not yet reported or dropped, for `dispose`. */
  private readonly live = new Set<Attempt>();

  constructor(private readonly host: CopyHost) {}

  /** Starts the copy of what a release selected, and returns its number
   * for the worker's answer. Safari writes to the clipboard only within
   * the user's gesture, which is over before the worker answers: the
   * write starts here with the text to come, and fails, leaving the
   * clipboard alone, when nothing was selected. Where the browser has no
   * `ClipboardItem` the text is written once it arrives. An earlier
   * release's copy still waiting is dropped, so the clipboard gets the
   * latest. */
  start(): number {
    for (const earlier of this.pending.values()) this.dropOne(earlier);
    this.pending.clear();
    const id = this.next++;
    const pending: Pending = {
      // At the timeout the write is left without its text.
      attempt: this.attempt("release", () => {
        this.pending.delete(id);
        pending.text?.reject();
      }),
    };
    this.pending.set(id, pending);
    const clipboard = this.host.clipboard();
    if (!this.host.item || !clipboard?.write) return id;
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
      return id;
    }
    pending.attempt.follow(clipboard.write([item]));
    return id;
  }

  /** The worker's answer to release `id`: the selected text, or undefined
   * when nothing was selected, which is no copy. An answer to a copy that
   * was dropped or timed out is ignored. */
  answer(id: number, text: string | undefined): void {
    const pending = this.pending.get(id);
    if (!pending) return;
    this.pending.delete(id);
    if (text === undefined) return this.dropOne(pending);
    pending.attempt.text(text);
    if (pending.text) pending.text.resolve(text);
    else this.writeText(pending.attempt, text);
  }

  /** Copies `text` known now. */
  write(text: string, source: CopySource): void {
    const attempt = this.attempt(source);
    attempt.text(text);
    this.writeText(attempt, text);
  }

  /** A copy the browser made itself (the `copy` event's data). */
  copied(text: string, source: CopySource): void {
    this.host.report({ ok: true, source, chars: length(text) });
  }

  /** Drops every copy under way, unreported. */
  dispose(): void {
    for (const pending of this.pending.values()) this.dropOne(pending);
    this.pending.clear();
    for (const attempt of this.live) attempt.drop();
  }

  private attempt(source: CopySource, timedOut?: () => void): Attempt {
    const attempt: Attempt = new Attempt(source, this.host.report, () => this.live.delete(attempt), timedOut);
    this.live.add(attempt);
    return attempt;
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
