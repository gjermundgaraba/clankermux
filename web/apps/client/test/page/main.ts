/** The browser suite's page: one client, terminals mounted from it by
 * name, and what they reported, for the suite to drive and read. */
import {
  type Client,
  ClientError,
  type Copy,
  type Event,
  type EventGap,
  type Link,
  type Terminal,
  type Until,
  type Waited,
  connect,
} from "@gjermundgaraba/clankermux";
import type { ToPage } from "@gjermundgaraba/clankermux/worker";

interface Mounted {
  terminal: Terminal;
  canvas: HTMLCanvasElement;
  links: Link[];
  statuses: string[];
  copies: Copy[];
}

let client: Client | undefined;
const mounted = new Map<string, Mounted>();
const events: (Event | EventGap)[] = [];
const clientLinks: Link[] = [];
const frames: Extract<ToPage, { type: "frame" }>[] = [];
const paints = new Map<number, FrameRequestCallback>();
let nextPaint = 0;

const harness = {
  async connect(url: string): Promise<void> {
    const worker = new Worker("./worker.js", { type: "module" });
    worker.addEventListener("message", ({ data }: MessageEvent<ToPage>) => {
      if (data.type === "frame") frames.push(data);
    });
    client = await connect({ url, worker, wasm: "./ghostty-vt.wasm" });
    client.onEvent((event) => events.push(event));
    client.onLink((link) => clientLinks.push(link));
  },

  mount(name: string, session: string, options: { ackOnFocus?: boolean } = {}): void {
    const container = document.createElement("div");
    container.className = "terminal";
    const canvas = document.createElement("canvas");
    canvas.id = name;
    canvas.tabIndex = 0;
    container.append(canvas);
    document.body.append(container);
    const terminal = client!.mount({
      canvas,
      container,
      session,
      font: "14px monospace",
      colors: () => ({ fg: "#24292e", bg: "#ffffff", cursor: "#24292e" }),
      ...options,
    });
    const entry: Mounted = { terminal, canvas, links: [], statuses: [], copies: [] };
    terminal.onLink((link) => entry.links.push(link));
    terminal.onCopy((copy) => entry.copies.push(copy));
    terminal.onStatus((text) => entry.statuses.push(text));
    mounted.set(name, entry);
  },

  frames: () => frames,
  pendingPaints: () => paints.size,
  holdPaints(): void {
    window.requestAnimationFrame = (callback) => {
      paints.set(++nextPaint, callback);
      return nextPaint;
    };
    window.cancelAnimationFrame = (id) => { paints.delete(id); };
  },
  paint(): void {
    const ready = [...paints.values()];
    paints.clear();
    for (const callback of ready) callback(performance.now());
  },

  link: (name: string): Link | undefined => mounted.get(name)?.links.at(-1),
  clientLinks: (): Link[] => clientLinks,
  /** What a listener added now gets at once. */
  latest(name: string): { link: Link | undefined; scroll: number | undefined; status: string | undefined } {
    const terminal = mounted.get(name)!.terminal;
    const seen: { link?: Link; scroll?: number; status?: string } = {};
    terminal.onLink((link) => (seen.link = link))();
    terminal.onScroll((rows) => (seen.scroll = rows))();
    terminal.onStatus((text) => (seen.status = text))();
    return { link: seen.link, scroll: seen.scroll, status: seen.status };
  },
  /** A wait pending when the client closes. */
  async closedWhileWaiting(session: string): Promise<{ kind: string } | undefined> {
    const waiting = client!.wait(session, { output: { text: "never-printed" } });
    await client!.list();
    client!.close();
    try {
      await waiting;
      return undefined;
    } catch (err) {
      return { kind: (err as ClientError).kind };
    }
  },
  statuses: (name: string): string[] => mounted.get(name)?.statuses ?? [],
  copies: (name: string): Copy[] => mounted.get(name)?.copies ?? [],
  events: (): (Event | EventGap)[] => events,

  /** A wait through the client; aborted after `abortMs` when given, and
   * before it starts when that is 0. */
  async wait(
    session: string,
    until: Until,
    abortMs?: number,
  ): Promise<{ waited: Waited } | { error: { name: string; kind: string; code: string | undefined } }> {
    const abort = new AbortController();
    if (abortMs === 0) abort.abort();
    else if (abortMs !== undefined) setTimeout(() => abort.abort(), abortMs);
    try {
      return { waited: await client!.wait(session, until, { signal: abort.signal }) };
    } catch (err) {
      if (!(err instanceof ClientError)) throw err;
      return { error: { name: err.name, kind: err.kind, code: err.code } };
    }
  },

  /** A wait started now, its outcome kept for `waited`; resolves once a
   * request sent after it was answered, so the wait is armed. */
  waiting: undefined as Promise<Waited> | undefined,
  async startWait(session: string, until: Until): Promise<void> {
    harness.waiting = client!.wait(session, until);
    await client!.list();
  },
  waited: (): Promise<Waited> => harness.waiting!,
};

export type Harness = typeof harness;

declare global {
  /** The page's harness, as the suite's `page.evaluate` calls it. */
  var harness: () => Harness;
}

globalThis.harness = () => harness;
