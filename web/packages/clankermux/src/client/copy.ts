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

export interface ClipboardLike {
  write?(items: ClipboardItem[]): Promise<void>;
  writeText?(text: string): Promise<void>;
}

export interface CopyHost {
  clipboard(): ClipboardLike | undefined;
  item: ((text: Promise<Blob>) => ClipboardItem) | undefined;
  report(copy: Copy): void;
}

const COPY_TIMEOUT_MS = 2000;

type Written = { ok: true } | { ok: false; reason: CopyFailure; message?: string };

const codePointLength = (text: string) => [...text].length;

class Attempt {
  private done = false;
  private chars: number | undefined;
  private result: Written | undefined;
  private readonly timer: ReturnType<typeof setTimeout>;

  constructor(
    private readonly source: CopySource,
    private readonly report: (copy: Copy) => void,
    private readonly ended: () => void,
    timedOut?: () => void,
  ) {
    this.timer = setTimeout(() => {
      this.finish({ ok: false, reason: "timeout" });
      timedOut?.();
    }, COPY_TIMEOUT_MS);
  }

  text(text: string): void {
    this.chars = codePointLength(text);
    this.settle();
  }

  wrote(result: Written): void {
    this.result ??= result;
    this.settle();
  }

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

  follow(write: Promise<void>): void {
    write.then(
      () => this.wrote({ ok: true }),
      (err: unknown) => this.wrote({ ok: false, reason: "refused", message: String(err) }),
    );
  }
}

interface Pending {
  id: number;
  attempt: Attempt;
  text?: { resolve: (text: string) => void; reject: () => void };
}

export class Copier {
  private pending: Pending | undefined;
  private next = 1;
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
    if (this.pending) this.dropOne(this.pending);
    const id = this.next++;
    const pending: Pending = {
      id,
      attempt: this.attempt("release", () => {
        if (this.pending === pending) this.pending = undefined;
        pending.text?.reject();
      }),
    };
    this.pending = pending;
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

  answer(id: number, text: string | undefined): void {
    const pending = this.pending;
    if (pending?.id !== id) return;
    this.pending = undefined;
    if (text === undefined) return this.dropOne(pending);
    pending.attempt.text(text);
    if (pending.text) pending.text.resolve(text);
    else this.writeText(pending.attempt, text);
  }

  write(text: string, source: CopySource): void {
    const attempt = this.attempt(source);
    attempt.text(text);
    this.writeText(attempt, text);
  }

  copied(text: string): void {
    this.host.report({ ok: true, source: "browser", chars: codePointLength(text) });
  }

  dispose(): void {
    if (this.pending) this.dropOne(this.pending);
    this.pending = undefined;
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
