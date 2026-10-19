/** The page: the session bar around a mounted terminal. */
import { mountTerminal } from "@clankermux/client";
import type { Session } from "@clankermux/proto";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const container = document.getElementById("terminal") as HTMLElement;
const sessionsSelect = document.getElementById("sessions") as HTMLSelectElement;
const newButton = document.getElementById("new") as HTMLButtonElement;
const status = document.getElementById("status") as HTMLElement;

const params = new URLSearchParams(location.search);
// The server's default WebSocket port on the host the page came from;
// `?ws=wss://<node>:<port>/` names any other.
const wsUrl = params.get("ws") ?? `wss://${location.hostname}:7438/`;

const style = getComputedStyle(document.documentElement);

const terminal = mountTerminal({
  worker: new Worker("./worker.js", { type: "module" }),
  canvas,
  container,
  wasm: "./ghostty-vt.wasm",
  url: wsUrl,
  session: params.get("session") ?? undefined,
  font: style.getPropertyValue("--font").trim() || "14px monospace",
  colors: () => ({
    fg: style.getPropertyValue("--fg"),
    bg: style.getPropertyValue("--bg"),
    cursor: style.getPropertyValue("--cursor"),
  }),
  onStatus: (text) => (status.textContent = text),
  onSessions: showSessions,
});

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
    terminal.attach(sessionsSelect.value);
    canvas.focus();
  }
};
newButton.onclick = () => {
  terminal.create();
  canvas.focus();
};
canvas.focus();
