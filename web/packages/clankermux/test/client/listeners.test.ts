import { expect, test, vi } from "vitest";

import { Listeners } from "../../src/client/listeners.ts";

test("replayLatest hands a late listener the latest value", () => {
  const listeners = new Listeners<number>();
  listeners.emit(1);
  const seen: number[] = [];
  listeners.add((n) => seen.push(n));
  listeners.emit(2);
  expect(seen).toEqual([1, 2]);
});

test("isolateErrors reaches every listener past one that throws, and replays nothing", async () => {
  const listeners = new Listeners<number>("isolateErrors");
  listeners.emit(1);
  const seen: number[] = [];
  listeners.add(() => {
    throw new Error("boom");
  });
  listeners.add((n) => seen.push(n));
  const rethrown = new Promise<unknown>((resolve) => {
    const original = globalThis.queueMicrotask;
    vi.spyOn(globalThis, "queueMicrotask").mockImplementationOnce((task) =>
      original(() => {
        try {
          task();
        } catch (err) {
          resolve(err);
        }
      }),
    );
  });
  listeners.emit(2);
  expect(seen).toEqual([2]);
  expect(await rethrown).toEqual(new Error("boom"));
  vi.restoreAllMocks();
});
