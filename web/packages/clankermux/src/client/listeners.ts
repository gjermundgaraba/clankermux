export type ListenerMode = "replayLatest" | "isolateErrors";

export class Listeners<T> {
  private readonly all = new Set<(value: T) => void>();
  private latest: { value: T } | undefined;
  private readonly mode: ListenerMode;

  constructor(mode: ListenerMode = "replayLatest") {
    this.mode = mode;
  }

  add(listener: (value: T) => void): () => void {
    this.all.add(listener);
    if (this.latest) listener(this.latest.value);
    return () => void this.all.delete(listener);
  }

  emit(value: T): void {
    if (this.mode === "replayLatest") {
      this.latest = { value };
      for (const listener of this.all) listener(value);
      return;
    }
    for (const listener of this.all) {
      try {
        listener(value);
      } catch (err) {
        queueMicrotask(() => {
          throw err;
        });
      }
    }
  }

  clear(): void {
    this.all.clear();
  }
}
