/** Listeners of one kind, and the latest value they were told. */
export class Listeners<T> {
  private readonly all = new Set<(value: T) => void>();
  private latest: { value: T } | undefined;

  add(listener: (value: T) => void): () => void {
    this.all.add(listener);
    if (this.latest) listener(this.latest.value);
    return () => void this.all.delete(listener);
  }

  emit(value: T): void {
    this.latest = { value };
    for (const listener of this.all) listener(value);
  }

  clear(): void {
    this.all.clear();
  }
}
