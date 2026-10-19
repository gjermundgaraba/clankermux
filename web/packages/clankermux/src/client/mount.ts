/** The page side of a terminal: canvas, keyboard, paste and resize, with
 * the engine in a worker running `runWorker`. */
import type { CellPoint } from "../engine/index.ts";
import type { Event, OnExit, Scheme, Session, Size } from "../proto/index.ts";
import { CanvasRenderer, type Metrics } from "../renderer/index.ts";

import { ClientError } from "./connection.ts";
import { type Keyboard, type Typed, Typing } from "./keyboard.ts";
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
  /** Drawn on. Focusing it focuses the terminal's input element, a
   * textarea placed after it, which takes keys, paste and composed text. */
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
  /** The rows between the bottom of the view and the live screen's, when
   * that changes: above 0 while the view is scrolled back into the
   * history (and growing with output meanwhile), 0 when it shows the
   * live screen again. The canvas draws a mark at its right edge too. */
  onScroll?: (rows: number) => void;
  /** Provisional, until the connection and the terminal are split: each
   * event that carries the attached session's `id` (`bell`,
   * `notification`, `progress`, `command_started`, `command_ended`,
   * `attachment_opened`, `attachment_closed`, the `program_status_*`
   * ones, `session_removed`). The others reach `onSessions`. */
  onEvent?: (event: Event) => void;
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
  /** Focuses the terminal's input element, as focusing the canvas does. */
  focus(): void;
  /** The selected text, as the last selection made copied it; undefined
   * when nothing is selected. */
  selection(): string | undefined;
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

  /** The selection's text, for a copy. */
  let selected: string | undefined;
  /** The clipboard write a release started, waiting for the worker to say
   * what was selected. */
  let copying: { resolve: (text: string) => void; reject: () => void } | undefined;
  /** Starts the copy of what a release selected. Safari writes to the
   * clipboard only within the user's gesture, which is over before the
   * worker answers: the write starts here with the text to come, and
   * fails, leaving the clipboard alone, when nothing was selected. */
  function startCopy(): void {
    copying?.reject();
    copying = undefined;
    if (typeof ClipboardItem === "undefined" || !navigator.clipboard?.write) return;
    let settle!: { resolve: (text: string) => void; reject: () => void };
    const text = new Promise<Blob>((resolve, reject) => {
      settle = {
        resolve: (value) => resolve(new Blob([value], { type: "text/plain" })),
        reject: () => reject(new Error("nothing selected")),
      };
    });
    navigator.clipboard.write([new ClipboardItem({ "text/plain": text })]).catch(() => {});
    const timer = setTimeout(() => settle.reject(), 2000);
    copying = {
      resolve: (value) => {
        clearTimeout(timer);
        settle.resolve(value);
      },
      reject: () => {
        clearTimeout(timer);
        settle.reject();
      },
    };
  }
  /** Rows the view is scrolled back, as `onScroll` last reported. */
  let scrolled = 0;
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
      case "frame": {
        if (message.frame.cursor) cursorAt = { x: message.frame.cursor.x, y: message.frame.cursor.y };
        const { total, offset, len } = message.frame.scrollbar;
        if (total - offset - len !== scrolled) {
          scrolled = total - offset - len;
          options.onScroll?.(scrolled);
        }
        fitCanvas(message.frame.cols, message.frame.rows);
        if (fullNext && !message.frame.full) {
          // The backing store was recreated; a partial frame cannot fill it.
          send({ type: "repaint" });
        } else {
          fullNext = false;
          renderer.draw(message.frame);
        }
        break;
      }
      case "sessions":
        options.onSessions?.(message.sessions, message.current);
        break;
      case "event":
        options.onEvent?.(message.event);
        break;
      case "clipboard":
        navigator.clipboard?.writeText(message.text).catch(() => {});
        break;
      case "selection": {
        selected = message.text;
        if (!message.copy) break;
        const copy = copying;
        copying = undefined;
        if (message.text === undefined) copy?.reject();
        else if (copy) copy.resolve(message.text);
        else navigator.clipboard?.writeText(message.text).catch(() => {});
        break;
      }
    }
  };

  /** Where the input element goes for a composition: the cursor's cell,
   * as the latest frame that had the cursor in view placed it. */
  let cursorAt = { x: 0, y: 0 };
  // An input method composes only into an editable element: a textarea
  // after the canvas takes the focus, invisible but at the cursor so the
  // input method's window opens there, and shows the composition while
  // there is one.
  const input = document.createElement("textarea");
  input.setAttribute("aria-label", "Terminal input");
  input.setAttribute("autocapitalize", "off");
  input.setAttribute("autocomplete", "off");
  input.setAttribute("autocorrect", "off");
  input.spellcheck = false;
  input.tabIndex = -1;
  const hidden = { opacity: "0", width: "1px", height: "1px" };
  Object.assign(input.style, {
    ...hidden,
    position: "absolute",
    margin: "0",
    padding: "0",
    border: "0",
    outline: "none",
    resize: "none",
    overflow: "hidden",
    whiteSpace: "pre",
    pointerEvents: "none",
    font: metrics.font,
    lineHeight: `${metrics.cellHeight}px`,
    caretColor: "transparent",
  });
  canvas.after(input);
  /** Moves the input element to the cursor, and shows it while composing. */
  function placeInput(composing: boolean): void {
    // Its position is against its containing block: a positioned
    // ancestor, or else the document.
    const cell = canvas.getBoundingClientRect();
    const parent = input.offsetParent;
    let left = cell.left + window.scrollX;
    let top = cell.top + window.scrollY;
    if (parent && getComputedStyle(parent).position !== "static") {
      const box = parent.getBoundingClientRect();
      left = cell.left - box.left - parent.clientLeft + parent.scrollLeft;
      top = cell.top - box.top - parent.clientTop + parent.scrollTop;
    }
    input.style.left = `${left + cursorAt.x * metrics.cellWidth}px`;
    input.style.top = `${top + cursorAt.y * metrics.cellHeight}px`;
    if (!composing) {
      Object.assign(input.style, hidden);
      return;
    }
    const { fg, bg } = colors();
    Object.assign(input.style, {
      opacity: "1",
      width: `${Math.max(1, grid.cols - cursorAt.x) * metrics.cellWidth}px`,
      height: `${metrics.cellHeight}px`,
      color: fg,
      background: bg,
    });
  }
  canvas.addEventListener("focus", () => input.focus({ preventScroll: true }), { signal });

  const typing = new Typing(keyboard);
  const typed = (what: Typed) => send("text" in what ? { type: "text", text: what.text } : { type: "key", input: what.key });
  input.addEventListener(
    "keydown",
    (e) => {
      // Ctrl+Shift+C copies a selection, where Ctrl+C is the terminal's;
      // without one it is a key.
      if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.code === "KeyC" && selected) {
        navigator.clipboard?.writeText(selected).catch(() => {});
        e.preventDefault();
        return;
      }
      const key = typing.keydown(e);
      if (key) {
        send({ type: "key", input: key });
        e.preventDefault();
      } else if (e.isComposing || e.key === "Dead" || e.keyCode === 229) {
        // At the cursor before the input method opens its window; shown
        // once a composition starts.
        placeInput(false);
      }
    },
    { signal },
  );
  input.addEventListener(
    "keyup",
    (e) => {
      const key = typing.keyup(e);
      if (!key) return;
      send({ type: "key", input: key });
      e.preventDefault();
    },
    { signal },
  );
  input.addEventListener(
    "beforeinput",
    (e) => {
      const what = typing.beforeinput(e);
      if (!what) return;
      typed(what);
      e.preventDefault();
    },
    { signal },
  );
  input.addEventListener("compositionstart", () => placeInput(true), { signal });
  input.addEventListener("blur", () => placeInput(false), { signal });
  input.addEventListener(
    "compositionend",
    (e) => {
      const what = typing.compositionend(e.data);
      if (what) typed(what);
      placeInput(false);
      // Emptied once the composition has left it.
      setTimeout(() => (input.value = ""));
    },
    { signal },
  );
  input.addEventListener(
    "input",
    (e) => {
      if (!(e as InputEvent).isComposing) input.value = "";
    },
    { signal },
  );
  input.addEventListener(
    "paste",
    (e) => {
      const text = e.clipboardData?.getData("text");
      if (text) send({ type: "paste", text });
      e.preventDefault();
    },
    { signal },
  );
  // The browser's copy (Cmd+C, Ctrl+C in a menu) copies the selection.
  // WebKit offers Copy on an empty textarea only when `beforecopy` is
  // cancelled.
  input.addEventListener(
    "beforecopy",
    (e) => {
      if (selected !== undefined) e.preventDefault();
    },
    { signal },
  );
  input.addEventListener(
    "copy",
    (e) => {
      if (selected === undefined || !e.clipboardData) return;
      e.clipboardData.setData("text/plain", selected);
      e.preventDefault();
    },
    { signal },
  );

  /** The pointer in cells of the grid, fractions included, kept inside it. */
  function cellAt(e: MouseEvent): CellPoint {
    const rect = canvas.getBoundingClientRect();
    const inside = (value: number, cells: number) => Math.min(Math.max(value, 0), Math.max(cells - 0.01, 0));
    return {
      x: inside((e.clientX - rect.left) / metrics.cellWidth, grid.cols),
      y: inside((e.clientY - rect.top) / metrics.cellHeight, grid.rows),
    };
  }
  // Selection: press, drag (once a frame) and release, followed outside
  // the canvas until the button is up. A release the page never saw (the
  // button let go over another window) ends the drag at the next move or
  // press.
  let endDrag: ((at: CellPoint, time: number) => void) | undefined;
  canvas.addEventListener(
    "mousedown",
    (e) => {
      e.preventDefault();
      input.focus({ preventScroll: true });
      if (e.button !== 0) return;
      endDrag?.(cellAt(e), e.timeStamp);
      send({ type: "select", action: "press", at: cellAt(e), time: e.timeStamp });
      let pending: CellPoint | undefined;
      let frame = 0;
      const flush = () => {
        cancelAnimationFrame(frame);
        frame = 0;
        if (pending) send({ type: "select", action: "drag", at: pending, time: 0 });
        pending = undefined;
      };
      const end = (at: CellPoint, time: number) => {
        flush();
        window.removeEventListener("mousemove", move);
        window.removeEventListener("mouseup", up);
        endDrag = undefined;
        startCopy();
        send({ type: "select", action: "release", at, time });
      };
      const move = (m: MouseEvent) => {
        if ((m.buttons & 1) === 0) return end(cellAt(m), m.timeStamp);
        pending = cellAt(m);
        frame ||= requestAnimationFrame(flush);
      };
      const up = (u: MouseEvent) => {
        if (u.button === 0) end(cellAt(u), u.timeStamp);
      };
      endDrag = end;
      window.addEventListener("mousemove", move, { signal });
      window.addEventListener("mouseup", up, { signal });
    },
    { signal },
  );
  // The wheel scrolls the history in whole rows; what a trackpad sends in
  // pixels adds up until it makes one. Ctrl+wheel stays the browser's zoom.
  let wheeled = 0;
  canvas.addEventListener(
    "wheel",
    (e) => {
      if (e.ctrlKey) return;
      e.preventDefault();
      const rows =
        e.deltaMode === WheelEvent.DOM_DELTA_LINE
          ? e.deltaY
          : e.deltaMode === WheelEvent.DOM_DELTA_PAGE
            ? e.deltaY * grid.rows
            : e.deltaY / metrics.cellHeight;
      wheeled += rows;
      const whole = Math.trunc(wheeled);
      if (whole === 0) return;
      wheeled -= whole;
      send({ type: "scroll", rows: whole });
    },
    { passive: false, signal },
  );

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
    focus: () => input.focus({ preventScroll: true }),
    selection: () => selected,
    report,
    paste: (text) => send({ type: "paste", text }),
    dispose: () => {
      disposed = true;
      listening.abort();
      resizes.disconnect();
      restyles.disconnect();
      input.remove();
      worker.onmessage = null;
      worker.terminate();
      for (const waiter of pending.values()) waiter.reject(new ClientError("closed", "disposed"));
      pending.clear();
    },
  };
}
