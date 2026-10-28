import type { CellPoint, PackedFrame } from "../engine/index.ts";
import type { Scheme, Size } from "../proto/index.ts";
import { CanvasRenderer, type Metrics } from "../renderer/index.ts";

import { type Copy, Copier } from "./copy.ts";
import { type Keyboard, type Typed, Typing, composing } from "./keyboard.ts";
import { Listeners } from "./listeners.ts";
import { profileFrom, schemeOf } from "./profile.ts";
import type { Link, ToPage, ToWorker } from "./worker.ts";

/** The terminal's default colours as CSS colours (hex or `rgb()`). */
export interface Colors {
  fg: string;
  bg: string;
  cursor: string;
}

export interface MountOptions {
  /** Drawn on. Focusing it focuses the terminal's input element, a
   * textarea placed after it, which takes keys, paste and composed text. */
  canvas: HTMLCanvasElement;
  /** The element whose size sets the grid. */
  container: HTMLElement;
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
  /** Attach read-only: the session's screen, with no input to it. */
  readOnly?: boolean;
  /** Mark the session's `done` and `error` records seen (`ack`) while the
   * terminal has the keyboard and its window the focus. */
  ackOnFocus?: boolean;
}

/** One mounted terminal: an attachment on its client's connection, drawn
 * on a canvas. */
export interface Terminal {
  /** Detaches the current session and attaches `session`; after the
   * client's connection ended for good, connects again first. */
  attach(session: string): void;
  /** Detaches the session; the terminal stays, attached to none. */
  detach(): void;
  /** Focuses the terminal's input element, as focusing the canvas does. */
  focus(): void;
  /** The selected text, as the last selection made copied it; undefined
   * when nothing is selected. */
  selection(): string | undefined;
  /** Pastes, bracketed when the program asked for it. */
  paste(text: string): void;
  /** Scrolls the view through the history by `rows`, up when negative. */
  scroll(rows: number): void;
  /** Back to the live screen. */
  scrollToLive(): void;
  /** Reports the container's size, the colours and the scheme again;
   * to the session only while the page is visible and focused. */
  report(): void;
  /** The grid the container holds now, for a `create` at this size. */
  size(): { cols: number; rows: number };
  /** Detaches, removes the listeners and the input element. */
  dispose(): void;
  /** Each change of the link to the session, to switch on; a listener
   * added later gets the latest at once. */
  onLink(listener: (link: Link) => void): () => void;
  /** The rows between the bottom of the view and the live screen's: on
   * the first frame, and at each change after. Above 0 while the view is
   * scrolled back into the history (and growing with output meanwhile),
   * 0 when it shows the live screen. The canvas draws a mark at its
   * right edge too. A listener added later gets the latest at once. */
  onScroll(listener: (rows: number) => void): () => void;
  /** A line of text for each change, for display; the latest at once. */
  onStatus(listener: (text: string) => void): () => void;
  /** Each copy to the clipboard as it ends: copied, with the text's
   * length, or failed and why. A release that selected nothing is no
   * copy. A listener added later is not told of earlier copies. */
  onCopy(listener: (copy: Copy) => void): () => void;
}

export interface MountHost {
  send(message: ToWorker): void;
  disposed(): void;
}

type DistributiveOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;
export type ToMount = DistributiveOmit<Extract<ToWorker, { mount: number }>, "mount">;

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

function isCopyChord(e: KeyboardEvent): boolean {
  return e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.code === "KeyC";
}

export function mountOn(
  host: MountHost,
  id: number,
  options: MountOptions,
): { terminal: Terminal; receive: (message: ToPage) => void } {
  const { canvas, container, colors } = options;
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

  let grid = { cols: 0, rows: 0, dpr: 0 };
  let backingStoreReset = false;
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
    backingStoreReset = true;
  }

  const keyboard: Keyboard = { layout: undefined, mac: /Mac|iPhone|iPad/.test(navigator.platform) };
  (navigator as { keyboard?: { getLayoutMap(): Promise<ReadonlyMap<string, string>> } }).keyboard
    ?.getLayoutMap()
    .then((map) => (keyboard.layout = map))
    .catch(() => {});

  let disposed = false;
  const send = (message: ToMount) => {
    if (!disposed) host.send({ ...message, mount: id } as ToWorker);
  };
  const links = new Listeners<Link>();
  const scrolls = new Listeners<number>();
  const statuses = new Listeners<string>();

  const copies = new Listeners<Copy>("isolateErrors");

  let selected: string | undefined;
  const copier = new Copier({
    clipboard: () => navigator.clipboard,
    item: typeof ClipboardItem === "undefined" ? undefined : (text) => new ClipboardItem({ "text/plain": text }),
    report: (copy) => copies.emit(copy),
  });
  let scrolled: number | undefined;
  const visible = () => document.visibilityState === "visible";
  let pendingFrame: PackedFrame | undefined;
  let paintRequest = 0;
  const paint = () => {
    paintRequest = 0;
    const frame = pendingFrame;
    pendingFrame = undefined;
    if (!frame || disposed) return;
    if (visible()) {
      fitCanvas(frame.cols, frame.rows);
      if (backingStoreReset && !frame.full) {
        send({ type: "repaint" });
      } else {
        backingStoreReset = false;
        renderer.draw(frame);
      }
    }
    send({ type: "painted" });
  };
  const receive = (message: ToPage) => {
    switch (message.type) {
      case "status":
        statuses.emit(message.text);
        break;
      case "link":
        links.emit(message.link);
        break;
      case "frame":
        if (disposed) break;
        if (!visible()) {
          send({ type: "painted" });
        } else {
          const frame = message.frame;
          if (frame.cursor) cursorAt = { x: frame.cursor.x, y: frame.cursor.y };
          const { total, offset, len } = frame.scrollbar;
          if (total - offset - len !== scrolled) {
            scrolled = total - offset - len;
            scrolls.emit(scrolled);
          }
          pendingFrame = frame;
          paintRequest = requestAnimationFrame(paint);
        }
        break;
      case "clipboard":
        copier.write(message.text, "program");
        break;
      case "selection":
        selected = message.text;
        if (message.copy !== undefined) copier.answer(message.copy, message.text);
        break;
    }
  };

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
  function placeInput(composing: boolean): void {
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
  let focused = false;
  const watchFocus = () => {
    const now = document.activeElement === input && document.hasFocus();
    if (now === focused) return;
    focused = now;
    send({ type: "focus", focused });
  };
  input.addEventListener("focus", watchFocus, { signal });
  input.addEventListener("blur", watchFocus, { signal });
  window.addEventListener("focus", watchFocus, { signal });
  window.addEventListener("blur", watchFocus, { signal });

  const typing = new Typing(keyboard);
  const typed = (what: Typed) => send("text" in what ? { type: "text", text: what.text } : { type: "key", input: what.key });
  input.addEventListener(
    "keydown",
    (e) => {
      if (isCopyChord(e) && selected) {
        copier.write(selected, "keys");
        e.preventDefault();
        return;
      }
      const key = typing.keydown(e);
      if (key) {
        send({ type: "key", input: key });
        e.preventDefault();
      } else if (composing(e)) {
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
      copier.copied(selected);
    },
    { signal },
  );

  function cellAt(e: MouseEvent): CellPoint {
    const rect = canvas.getBoundingClientRect();
    const inside = (value: number, cells: number) => Math.min(Math.max(value, 0), Math.max(cells - 0.01, 0));
    return {
      x: inside((e.clientX - rect.left) / metrics.cellWidth, grid.cols),
      y: inside((e.clientY - rect.top) / metrics.cellHeight, grid.rows),
    };
  }
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
        send({ type: "select", action: "release", at, time, copy: copier.start() });
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
      send({ type: "wheel", rows: whole });
    },
    { passive: false, signal },
  );

  let reported = "";
  const profile = () => {
    const current = profileFrom(colors(), scheme());
    reported = JSON.stringify(current);
    return current;
  };
  const active = () => visible() && document.hasFocus();
  const report = () => send({ type: "viewport", size: viewport(), profile: profile(), active: active() });
  const resizes = new ResizeObserver(report);
  resizes.observe(container);
  light.addEventListener("change", report, { signal });
  const releaseFrameWhileHidden = () => {
    if (visible() || !pendingFrame) return;
    cancelAnimationFrame(paintRequest);
    paintRequest = 0;
    pendingFrame = undefined;
    send({ type: "painted" });
  };
  document.addEventListener("visibilitychange", () => {
    send({ type: "visibility", visible: visible() });
    releaseFrameWhileHidden();
    report();
  }, { signal });
  window.addEventListener("focus", report, { signal });
  window.addEventListener("blur", report, { signal });
  const restyles = new MutationObserver(() => {
    if (JSON.stringify(profileFrom(colors(), scheme())) !== reported) report();
  });
  for (let element: Element | null = canvas; element; element = element.parentElement) {
    restyles.observe(element, { attributes: true });
  }
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
    type: "mount",
    session: options.session,
    viewport: viewport(),
    profile: profile(),
    active: active(),
    visible: visible(),
    readOnly: options.readOnly ?? false,
    ackOnFocus: options.ackOnFocus ?? false,
  });

  const terminal: Terminal = {
    attach: (session) => send({ type: "attach", session }),
    detach: () => send({ type: "detach" }),
    focus: () => input.focus({ preventScroll: true }),
    selection: () => selected,
    paste: (text) => send({ type: "paste", text }),
    scroll: (rows) => send({ type: "view", rows }),
    scrollToLive: () => send({ type: "view", rows: "live" }),
    report,
    size: () => {
      const { cols, rows } = viewport();
      return { cols, rows };
    },
    dispose: () => {
      if (disposed) return;
      host.send({ type: "unmount", mount: id });
      disposed = true;
      cancelAnimationFrame(paintRequest);
      pendingFrame = undefined;
      host.disposed();
      listening.abort();
      resizes.disconnect();
      restyles.disconnect();
      input.remove();
      links.clear();
      scrolls.clear();
      statuses.clear();
      copier.dispose();
      copies.clear();
    },
    onLink: (listener) => links.add(listener),
    onScroll: (listener) => scrolls.add(listener),
    onStatus: (listener) => statuses.add(listener),
    onCopy: (listener) => copies.add(listener),
  };
  return { terminal, receive };
}
