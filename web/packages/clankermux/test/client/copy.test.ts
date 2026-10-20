import { afterEach, beforeEach, expect, test, vi } from "vitest";

import { type ClipboardLike, type Copy, Copier, type CopyHost } from "../../src/client/copy.ts";

beforeEach(() => void vi.useFakeTimers());
afterEach(() => void vi.useRealTimers());

/** A clipboard as a browser has it: `write` reads the item's text, then
 * does what `refuse` says, never settling when it is "hang". */
function clipboard(refuse?: Error | "hang"): ClipboardLike & { written: string[] } {
  const written: string[] = [];
  const settle = async (text: string) => {
    if (refuse === "hang") return new Promise<void>(() => {});
    if (refuse) throw refuse;
    written.push(text);
  };
  return {
    written,
    async write(items) {
      const blob = await (items[0] as unknown as { text: Promise<Blob> }).text;
      await settle(await blob.text());
    },
    writeText: settle,
  };
}

function copier(board: ClipboardLike | undefined, item = true): { copier: Copier; copies: Copy[] } {
  const copies: Copy[] = [];
  const host: CopyHost = {
    clipboard: () => board,
    item: item ? (text) => ({ text }) as unknown as ClipboardItem : undefined,
    report: (copy) => copies.push(copy),
  };
  return { copier: new Copier(host), copies };
}

const settled = () => vi.advanceTimersByTimeAsync(0);
const refused = Object.assign(new Error("Document is not focused."), { name: "NotAllowedError" });

test("a release's copy is told once the write is done, with the text's length", async () => {
  const board = clipboard();
  const { copier: c, copies } = copier(board);
  c.start();
  c.answer("héllo \u{1f389}\n");
  await settled();
  expect(board.written).toEqual(["héllo \u{1f389}\n"]);
  expect(copies).toEqual([{ ok: true, source: "release", chars: 8 }]);
});

test("a release that selected nothing is no copy, and leaves the clipboard alone", async () => {
  const board = clipboard();
  const { copier: c, copies } = copier(board);
  c.start();
  c.answer(undefined);
  await vi.advanceTimersByTimeAsync(5000);
  expect(board.written).toEqual([]);
  expect(copies).toEqual([]);
});

test("a refused write is told with the browser's reason, once the text's length is known", async () => {
  const { copier: c, copies } = copier(clipboard(refused));
  c.start();
  await settled();
  expect(copies).toEqual([]);
  c.answer("abc");
  await settled();
  expect(copies).toEqual([
    { ok: false, source: "release", reason: "refused", chars: 3, message: "NotAllowedError: Document is not focused." },
  ]);
  // Refused, then nothing selected: no copy either.
  c.start();
  c.answer(undefined);
  await vi.advanceTimersByTimeAsync(5000);
  expect(copies).toHaveLength(1);
});

test("a copy that does not finish in two seconds times out, and is told once", async () => {
  // The worker never answers.
  const board = clipboard();
  const { copier: c, copies } = copier(board);
  c.start();
  await vi.advanceTimersByTimeAsync(1999);
  expect(copies).toEqual([]);
  await vi.advanceTimersByTimeAsync(1);
  expect(copies).toEqual([{ ok: false, source: "release", reason: "timeout" }]);
  c.answer("late");
  await vi.advanceTimersByTimeAsync(5000);
  expect(copies).toHaveLength(1);
  expect(board.written).toEqual([]);

  // The write never settles: its length is known.
  const hung = copier(clipboard("hang"));
  hung.copier.start();
  hung.copier.answer("abc");
  hung.copier.write("keys", "keys");
  await vi.advanceTimersByTimeAsync(2000);
  expect(hung.copies).toEqual([
    { ok: false, source: "release", reason: "timeout", chars: 3 },
    { ok: false, source: "keys", reason: "timeout", chars: 4 },
  ]);
});

test("a release while an earlier one waits drops the earlier, and the answers go to theirs", async () => {
  const board = clipboard();
  const { copier: c, copies } = copier(board);
  c.start();
  c.start();
  c.answer("first");
  c.answer("second!");
  await settled();
  expect(board.written).toEqual(["second!"]);
  expect(copies).toEqual([{ ok: true, source: "release", chars: 7 }]);
});

test("without ClipboardItem the text is written once it arrives; without a clipboard the copy is unavailable", async () => {
  const board = clipboard();
  const plain = copier(board, false);
  plain.copier.start();
  plain.copier.answer("abc");
  await settled();
  expect(board.written).toEqual(["abc"]);
  expect(plain.copies).toEqual([{ ok: true, source: "release", chars: 3 }]);

  // A ClipboardItem that takes no text to come, as older browsers have.
  const older = clipboard();
  const copies: Copy[] = [];
  const c = new Copier({
    clipboard: () => older,
    item: () => {
      throw new TypeError("not a string or blob");
    },
    report: (copy) => copies.push(copy),
  });
  c.start();
  c.answer("abc");
  await settled();
  expect(older.written).toEqual(["abc"]);
  expect(copies).toEqual([{ ok: true, source: "release", chars: 3 }]);

  const none = copier(undefined);
  none.copier.start();
  none.copier.answer(undefined);
  none.copier.start();
  none.copier.answer("abc");
  none.copier.write("x", "program");
  await settled();
  expect(none.copies).toEqual([
    { ok: false, source: "release", reason: "unavailable", chars: 3 },
    { ok: false, source: "program", reason: "unavailable", chars: 1 },
  ]);
});

test("copies of text known at once: the keys, the program's, and the browser's own", async () => {
  const board = clipboard();
  const { copier: c, copies } = copier(board);
  c.write("ab", "keys");
  c.write("osc", "program");
  c.copied("menu", "browser");
  await settled();
  expect(board.written).toEqual(["ab", "osc"]);
  expect(copies).toEqual([
    { ok: true, source: "browser", chars: 4 },
    { ok: true, source: "keys", chars: 2 },
    { ok: true, source: "program", chars: 3 },
  ]);
  const no = copier(clipboard(refused));
  no.copier.write("ab", "keys");
  await settled();
  expect(no.copies).toEqual([{ ok: false, source: "keys", reason: "refused", chars: 2, message: "NotAllowedError: Document is not focused." }]);
});

test("dispose drops a copy waiting for its text, untold", async () => {
  const { copier: c, copies } = copier(clipboard());
  c.start();
  c.dispose();
  c.answer("abc");
  await vi.advanceTimersByTimeAsync(5000);
  expect(copies).toEqual([]);
});
