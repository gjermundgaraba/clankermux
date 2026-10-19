/** The page: the session bar around a mounted terminal. */
import { type Link, type Session, mountTerminal } from "@gjermundgaraba/clankermux";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const container = document.getElementById("terminal") as HTMLElement;
const sessionsSelect = document.getElementById("sessions") as HTMLSelectElement;
const newButton = document.getElementById("new") as HTMLButtonElement;
const endButton = document.getElementById("end") as HTMLButtonElement;
const removeButton = document.getElementById("remove") as HTMLButtonElement;
const schemeSelect = document.getElementById("scheme") as HTMLSelectElement;
const status = document.getElementById("status") as HTMLElement;
const linkText = document.getElementById("link") as HTMLElement;
const where = document.getElementById("where") as HTMLElement;

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
  onLink: (link) => (linkText.textContent = describeLink(link)),
  onSessions: showSessions,
});

/** The link in a few words, so the page shows what a consumer would
 * switch on. */
function describeLink(link: Link): string {
  switch (link.state) {
    case "connecting":
      return `connecting (${link.attempt})`;
    case "connected":
      return link.session ? `connected, ${link.session.status}` : "connected, no session";
    case "ended":
      return `ended: ${link.reason}${link.code ? ` (${link.code})` : ""}${link.retry ? ", retrying" : ""}`;
  }
}

const failed = (err: unknown) => (status.textContent = String(err));

function showSessions(sessions: Session[], current: string | undefined): void {
  sessionsSelect.replaceChildren(
    new Option("choose a session", "", current === undefined, current === undefined),
    ...sessions.map((s) => {
      const text = `${s.id.slice(0, 8)} ${s.label || s.title || s.argv.join(" ")} (${s.status})`;
      return new Option(text, s.id, s.id === current, s.id === current);
    }),
  );
  // The status line: `sessions` follows the program's title (OSC 0/2),
  // working directory (OSC 7) and the session's size as they change. The
  // size is the one of the page last visible and focused.
  const attached = sessions.find((s) => s.id === current);
  where.textContent = attached
    ? [attached.title, attached.pwd, `${attached.cols}\u00d7${attached.rows}`].filter(Boolean).join(" \u2014 ")
    : "";
}

sessionsSelect.onchange = () => {
  if (sessionsSelect.value) {
    history.replaceState(null, "", `?${new URLSearchParams({ ...Object.fromEntries(params), session: sessionsSelect.value })}`);
    terminal.attach(sessionsSelect.value);
    canvas.focus();
  }
};
newButton.onclick = () => {
  terminal.create().catch(failed);
  canvas.focus();
};
endButton.onclick = () => terminal.end().catch(failed);
removeButton.onclick = () => terminal.remove().catch(failed);
// Pins the page's scheme; the terminal notices the attribute and reports
// the canvas's scheme and colours again.
schemeSelect.onchange = () => {
  if (schemeSelect.value) document.documentElement.dataset["scheme"] = schemeSelect.value;
  else delete document.documentElement.dataset["scheme"];
};
canvas.focus();
