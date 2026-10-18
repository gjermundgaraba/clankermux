/** The page: canvas, keyboard, paste, resize and the session bar. */
import { CanvasRenderer, type Metrics } from "@clankermux/renderer";
import type { Session, Size } from "@clankermux/proto";

import { type Keyboard, browserKeeps, keyInput } from "./keyboard.ts";
import { profileFrom } from "./profile.ts";
import type { ToPage, ToWorker } from "./protocol.ts";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const container = document.getElementById("terminal") as HTMLElement;
const sessionsSelect = document.getElementById("sessions") as HTMLSelectElement;
const newButton = document.getElementById("new") as HTMLButtonElement;
const status = document.getElementById("status") as HTMLElement;

const params = new URLSearchParams(location.search);
const wsUrl = params.get("ws") ?? `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`;

const style = getComputedStyle(document.documentElement);
const font = style.getPropertyValue("--font").trim() || "14px monospace";
const colors = () => ({
  fg: style.getPropertyValue("--fg"),
  bg: style.getPropertyValue("--bg"),
  cursor: style.getPropertyValue("--cursor"),
});
const scheme = () => (matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark");

/** Cell metrics from the font. */
function measure(): Metrics {
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

const metrics = measure();
const ctx = canvas.getContext("2d")!;
const renderer = new CanvasRenderer(ctx, metrics, { cursor: colors().cursor });
const dpr = () => window.devicePixelRatio || 1;

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

const worker = new Worker("./worker.js", { type: "module" });
const send = (message: ToWorker) => worker.postMessage(message);

worker.onmessage = (e: MessageEvent<ToPage>) => {
  const message = e.data;
  switch (message.type) {
    case "status":
      status.textContent = message.text;
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
      showSessions(message.sessions, message.current);
      break;
    case "clipboard":
      navigator.clipboard?.writeText(message.text).catch(() => {});
      break;
  }
};

function showSessions(sessions: Session[], current: string | undefined): void {
  sessionsSelect.replaceChildren(
    new Option("choose a session", "", current === undefined, current === undefined),
    ...sessions.map((s) => {
      const text = `${s.id.slice(0, 8)} ${s.label || s.title || s.argv.join(" ")} (${s.status})`;
      return new Option(text, s.id, s.id === current, s.id === current);
    }),
  );
}

sessionsSelect.onchange = () => {
  if (sessionsSelect.value) {
    history.replaceState(null, "", `?${new URLSearchParams({ ...Object.fromEntries(params), session: sessionsSelect.value })}`);
    send({ type: "attach", session: sessionsSelect.value });
    canvas.focus();
  }
};
newButton.onclick = () => {
  send({ type: "create" });
  canvas.focus();
};

for (const type of ["keydown", "keyup"] as const) {
  canvas.addEventListener(type, (e) => {
    if (browserKeeps(e)) return;
    send({ type: "key", input: keyInput(e, keyboard) });
    e.preventDefault();
  });
}
canvas.addEventListener("paste", (e) => {
  const text = e.clipboardData?.getData("text");
  if (text) send({ type: "paste", text });
  e.preventDefault();
});
canvas.addEventListener("mousedown", () => canvas.focus());

const report = () => send({ type: "viewport", size: viewport(), profile: profileFrom(colors(), scheme()) });
new ResizeObserver(report).observe(container);
matchMedia("(prefers-color-scheme: light)").addEventListener("change", report);
// Zoom or a move to another screen changes the pixel ratio: the canvas
// is refitted on the next frame; the cell size in pixels is re-reported.
const watchRatio = () => {
  matchMedia(`(resolution: ${dpr()}dppx)`).addEventListener("change", () => {
    send({ type: "repaint" });
    report();
    watchRatio();
  }, { once: true });
};
watchRatio();

send({
  type: "init",
  wasm: new URL("./ghostty-vt.wasm", location.href).toString(),
  url: wsUrl,
  viewport: viewport(),
  profile: profileFrom(colors(), scheme()),
  session: params.get("session") ?? undefined,
});
canvas.focus();
