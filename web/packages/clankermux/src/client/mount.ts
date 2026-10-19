/** The page side of a terminal: canvas, keyboard, paste and resize, with
 * the engine in a worker running `runWorker`. */
import type { OnExit, Scheme, Session, Size } from "../proto/index.ts";
import { CanvasRenderer, type Metrics } from "../renderer/index.ts";

import { ClientError } from "./connection.ts";
import { type Keyboard, browserKeeps, keyInput } from "./keyboard.ts";
import { profileFrom, schemeOf } from "./profile.ts";
import type { Failure, Link, ToPage, ToWorker } from "./worker.ts";

/** The terminal's default colours as CSS colours (hex or `rgb()`). */
export interface Colors {
  fg: string;
  bg: string;
  cursor: string;
}

export interface MountOptions {
  /** A module worker whose script calls `runWorker(self)`; the terminal
   * owns it from here. */
  worker: Worker;
  /** Drawn on; takes keys and paste while focused. */
  canvas: HTMLCanvasElement;
  /** The element whose size sets the grid. */
  container: HTMLElement;
  /** ghostty-vt.wasm, resolved against the page. */
  wasm: string;
  /** The server's WebSocket carrier, `wss://<node>:<port>/`. */
  url: string;
  /** The session to attach at once. */
  session?: string | undefined;
  /** A CSS font shorthand with its size in px. */
  font: string;
  /** Read again on every report. */
  colors: () => Colors;
  /** The colour scheme reported to the session. By default the canvas's
   * own: the scheme its computed `color-scheme` names, when it names one
   * of `light` and `dark`, else the page's preferred scheme. Read again
   * on every report. */
  scheme?: () => Scheme;
  /** A line of text for each change, for display. */
  onStatus?: (text: string) => void;
  /** Each change of the link to the session, to switch on. */
  onLink?: (link: Link) => void;
  onSessions?: (sessions: Session[], current: string | undefined) => void;
}

export interface CreateOptions {
  /** `keep`: the session stays listed as `exited` when its program
   * exits; `remove`: it is removed `limits.remove_grace` after. The
   * server's default (`remove`) when absent. */
  onExit?: OnExit;
}

export interface MountedTerminal {
  /** Detaches the current session and attaches `session`. */
  attach(session: string): void;
  /** Creates a session at the current size and attaches it; resolves
   * with it as created. Like `end` and `remove`, it needs a connection:
   * call it after a `connected` link. */
  create(options?: CreateOptions): Promise<Session>;
  /** Hangs up the attached session's processes (SIGHUP); its exit
   * follows as a link `ended` with reason `exited`. Resolves too when the
   * session is not running (`exited` or `restored`). */
  end(): Promise<void>;
  /** Removes the attached session, which must be `exited` or `restored`
   * (else `running`); the link ends with `removed`. */
  remove(): Promise<void>;
  /** Reports the container's size, the colours and the scheme again;
   * to the session only while the page is visible and focused. */
  report(): void;
  paste(text: string): void;
  /** Removes the listeners and terminates the worker, which ends its
   * connection; requests in flight reject. */
  dispose(): void;
}

/** Cell metrics from the font. */
function measure(font: string): Metrics {
  const ctx = document.createElement("canvas").getContext("2d")!;
  ctx.font = font;
  const m = ctx.measureText("M");
  const size = Number(/(\d+(?:\.\d+)?)px/.exec(font)?.[1] ?? 14);
  const ascent = m.fontBoundingBoxAscent || size * 0.8;
  const descent = m.fontBoundingBoxDescent || size * 0.25;
  return {
    cellWidth: Math.ceil(m.width),
    cellHeight: Math.ceil(ascent + descent),
    baseline: Math.round(ascent),
    font,
    boldFont: `bold ${font}`,
    italicFont: `italic ${font}`,
    boldItalicFont: `bold italic ${font}`,
  };
}

export function mountTerminal(options: MountOptions): MountedTerminal {
  const { worker, canvas, container, colors } = options;
  const light = matchMedia("(prefers-color-scheme: light)");
  const scheme = options.scheme ?? (() => schemeOf(getComputedStyle(canvas).colorScheme, light.matches));
  const metrics = measure(options.font);
  const ctx = canvas.getContext("2d")!;
  const renderer = new CanvasRenderer(ctx, metrics, { cursor: colors().cursor });
  const dpr = () => window.devicePixelRatio || 1;
  const listening = new AbortController();
  const signal = listening.signal;

  function viewport(): Size {
    const rect = container.getBoundingClientRect();
    return {
      cols: Math.max(1, Math.floor(rect.width / metrics.cellWidth)),
      rows: Math.max(1, Math.floor(rect.height / metrics.cellHeight)),
      cell_width: Math.round(metrics.cellWidth * dpr()),
      cell_height: Math.round(metrics.cellHeight * dpr()),
    };
  }

  /** The canvas backing store for the grid at the current pixel ratio;
   * the next frame is drawn in full after a change. */
  let grid = { cols: 0, rows: 0, dpr: 0 };
  let fullNext = false;
  function fitCanvas(cols: number, rows: number): void {
    const scale = dpr();
    if (grid.cols === cols && grid.rows === rows && grid.dpr === scale) return;
    grid = { cols, rows, dpr: scale };
    canvas.width = cols * metrics.cellWidth * scale;
    canvas.height = rows * metrics.cellHeight * scale;
    canvas.style.width = `${cols * metrics.cellWidth}px`;
    canvas.style.height = `${rows * metrics.cellHeight}px`;
    ctx.setTransform(scale, 0, 0, scale, 0, 0);
    ctx.textBaseline = "alphabetic";
    fullNext = true;
  }

  /** The keyboard layout, when the browser exposes it (Chromium does). */
  const keyboard: Keyboard = { layout: undefined, mac: /Mac|iPhone|iPad/.test(navigator.platform) };
  (navigator as { keyboard?: { getLayoutMap(): Promise<ReadonlyMap<string, string>> } }).keyboard
    ?.getLayoutMap()
    .then((map) => (keyboard.layout = map))
    .catch(() => {});

  const send = (message: ToWorker) => worker.postMessage(message);

  /** Requests waiting for their `done`, by id. They reject with a
   * `ClientError` for a server's refusal, a lost connection or a
   * disposed terminal, or an `Error` when no session is attached. */
  const pending = new Map<number, { resolve: (session: Session | undefined) => void; reject: (err: Error) => void }>();
  let nextId = 1;
  let disposed = false;
  function request(message: (id: number) => ToWorker): Promise<Session | undefined> {
    if (disposed) return Promise.reject(new ClientError("closed", "disposed"));
    const id = nextId++;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      send(message(id));
    });
  }
  const failure = (error: Failure): Error =>
    error.kind ? new ClientError(error.kind, error.message, error.server) : new Error(error.message);

  worker.onmessage = (e: MessageEvent<ToPage>) => {
    const message = e.data;
    switch (message.type) {
      case "status":
        options.onStatus?.(message.text);
        break;
      case "link":
        options.onLink?.(message.link);
        break;
      case "done": {
        const waiter = pending.get(message.id);
        pending.delete(message.id);
        if (message.error) waiter?.reject(failure(message.error));
        else waiter?.resolve(message.session);
        break;
      }
      case "frame":
        fitCanvas(message.frame.cols, message.frame.rows);
        if (fullNext && !message.frame.full) {
          // The backing store was recreated; a partial frame cannot fill it.
          send({ type: "repaint" });
        } else {
          fullNext = false;
          renderer.draw(message.frame);
        }
        break;
      case "sessions":
        options.onSessions?.(message.sessions, message.current);
        break;
      case "clipboard":
        navigator.clipboard?.writeText(message.text).catch(() => {});
        break;
    }
  };

  for (const type of ["keydown", "keyup"] as const) {
    canvas.addEventListener(
      type,
      (e) => {
        if (browserKeeps(e)) return;
        send({ type: "key", input: keyInput(e, keyboard) });
        e.preventDefault();
      },
      { signal },
    );
  }
  canvas.addEventListener(
    "paste",
    (e) => {
      const text = e.clipboardData?.getData("text");
      if (text) send({ type: "paste", text });
      e.preventDefault();
    },
    { signal },
  );
  canvas.addEventListener("mousedown", () => canvas.focus(), { signal });

  /** The profile last reported, to tell when it changed. */
  let reported = "";
  const profile = () => {
    const current = profileFrom(colors(), scheme());
    reported = JSON.stringify(current);
    return current;
  };
  /** Whether the user is looking at this page: it is visible and its
   * window has the focus. Only then does the session get its size, so
   * of several clients the one in use sets it. */
  const active = () => document.visibilityState === "visible" && document.hasFocus();
  const report = () => send({ type: "viewport", size: viewport(), profile: profile(), active: active() });
  const resizes = new ResizeObserver(report);
  resizes.observe(container);
  light.addEventListener("change", report, { signal });
  // Shown or focused: the session takes this page's size again; hidden or
  // blurred: the worker stops reporting.
  document.addEventListener("visibilitychange", report, { signal });
  window.addEventListener("focus", report, { signal });
  window.addEventListener("blur", report, { signal });
  // A page pins its scheme (and its colours with it) with an attribute or
  // a class on the canvas or an ancestor, which no event announces.
  const restyles = new MutationObserver(() => {
    if (JSON.stringify(profileFrom(colors(), scheme())) !== reported) report();
  });
  for (let element: Element | null = canvas; element; element = element.parentElement) {
    restyles.observe(element, { attributes: true });
  }
  // Zoom or a move to another screen changes the pixel ratio: the canvas
  // is refitted on the next frame; the cell size in pixels is re-reported.
  const watchRatio = () => {
    matchMedia(`(resolution: ${dpr()}dppx)`).addEventListener(
      "change",
      () => {
        send({ type: "repaint" });
        report();
        watchRatio();
      },
      { once: true, signal },
    );
  };
  watchRatio();

  send({
    type: "init",
    wasm: new URL(options.wasm, location.href).toString(),
    url: options.url,
    viewport: viewport(),
    profile: profile(),
    active: active(),
    session: options.session,
  });

  return {
    attach: (session) => send({ type: "attach", session }),
    create: async (create) => {
      const session = await request((id) => ({ type: "create", id, ...(create?.onExit && { onExit: create.onExit }) }));
      return session!;
    },
    end: async () => void (await request((id) => ({ type: "end", id }))),
    remove: async () => void (await request((id) => ({ type: "remove", id }))),
    report,
    paste: (text) => send({ type: "paste", text }),
    dispose: () => {
      disposed = true;
      listening.abort();
      resizes.disconnect();
      restyles.disconnect();
      worker.onmessage = null;
      worker.terminate();
      for (const waiter of pending.values()) waiter.reject(new ClientError("closed", "disposed"));
      pending.clear();
    },
  };
}
