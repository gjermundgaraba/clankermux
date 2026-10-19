/** The page side of a terminal: canvas, keyboard, paste and resize, with
 * the engine in a worker running `runWorker`. */
import type { Scheme, Session, Size } from "../proto/index.ts";
import { CanvasRenderer, type Metrics } from "../renderer/index.ts";

import { type Keyboard, browserKeeps, keyInput } from "./keyboard.ts";
import { profileFrom } from "./profile.ts";
import type { ToPage, ToWorker } from "./worker.ts";

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
  /** The colour scheme reported to the session; the page's preferred
   * scheme by default. */
  scheme?: () => Scheme;
  onStatus?: (text: string) => void;
  onSessions?: (sessions: Session[], current: string | undefined) => void;
}

export interface MountedTerminal {
  /** Detaches the current session and attaches `session`. */
  attach(session: string): void;
  /** Creates a session at the current size and attaches it. */
  create(): void;
  /** Reports the container's size, the colours and the scheme again. */
  report(): void;
  paste(text: string): void;
  /** Removes the listeners and terminates the worker, which ends its
   * connection. */
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
  const scheme = options.scheme ?? (() => (light.matches ? "light" : "dark"));
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

  worker.onmessage = (e: MessageEvent<ToPage>) => {
    const message = e.data;
    switch (message.type) {
      case "status":
        options.onStatus?.(message.text);
        break;
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

  const report = () => send({ type: "viewport", size: viewport(), profile: profileFrom(colors(), scheme()) });
  const resizes = new ResizeObserver(report);
  resizes.observe(container);
  light.addEventListener("change", report, { signal });
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
    profile: profileFrom(colors(), scheme()),
    session: options.session,
  });

  return {
    attach: (session) => send({ type: "attach", session }),
    create: () => send({ type: "create" }),
    report,
    paste: (text) => send({ type: "paste", text }),
    dispose: () => {
      listening.abort();
      resizes.disconnect();
      worker.onmessage = null;
      worker.terminate();
    },
  };
}
