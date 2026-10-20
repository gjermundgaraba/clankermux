/** The page: the session bar around a terminal mounted from a client. */
import { type Link, type Session, connect } from "@gjermundgaraba/clankermux";

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
const scrolled = document.getElementById("scrolled") as HTMLElement;

const params = new URLSearchParams(location.search);
// The server's default WebSocket port on the host the page came from;
// `?ws=wss://<node>:<port>/` names any other.
const wsUrl = params.get("ws") ?? `wss://${location.hostname}:7438/`;

const style = getComputedStyle(document.documentElement);

const client = await connect({
  worker: new Worker("./worker.js", { type: "module" }),
  wasm: "./ghostty-vt.wasm",
  url: wsUrl,
});
/** The session shown, as the page last attached or created it. */
let current = params.get("session") ?? undefined;
const terminal = client.mount({
  canvas,
  container,
  session: current,
  font: style.getPropertyValue("--font").trim() || "14px monospace",
  colors: () => ({
    fg: style.getPropertyValue("--fg"),
    bg: style.getPropertyValue("--bg"),
    cursor: style.getPropertyValue("--cursor"),
  }),
  ackOnFocus: true,
});
terminal.onStatus((text) => (status.textContent = text));
terminal.onLink((link) => (linkText.textContent = describeLink(link)));
// Scrolled back with the wheel or Shift+PageUp: how far above the live
// screen the view ends.
terminal.onScroll((rows) => {
  scrolled.hidden = rows === 0;
  scrolled.textContent = `\u2191 ${rows} rows`;
});
let listed: Session[] = [];
client.onSessions((sessions) => {
  listed = sessions;
  showSessions();
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

function showSessions(): void {
  sessionsSelect.replaceChildren(
    new Option("choose a session", "", current === undefined, current === undefined),
    ...listed.map((s) => {
      const text = `${s.id.slice(0, 8)} ${s.label || s.title || s.argv.join(" ")} (${s.status})`;
      return new Option(text, s.id, s.id === current, s.id === current);
    }),
  );
  // The status line: `sessions` follows the program's title (OSC 0/2),
  // working directory (OSC 7) and the session's size as they change. The
  // size is the one of the page last visible and focused.
  const attached = listed.find((s) => s.id === current);
  where.textContent = attached
    ? [attached.title, attached.pwd, `${attached.cols}\u00d7${attached.rows}`].filter(Boolean).join(" \u2014 ")
    : "";
}

function show(session: string): void {
  current = session;
  history.replaceState(null, "", `?${new URLSearchParams({ ...Object.fromEntries(params), session })}`);
  terminal.attach(session);
  showSessions();
}

sessionsSelect.onchange = () => {
  if (sessionsSelect.value) {
    show(sessionsSelect.value);
    canvas.focus();
  }
};
newButton.onclick = () => {
  client
    .create(terminal.size())
    .then((session) => show(session.id))
    .catch(failed);
  canvas.focus();
};
// `not_running`: there is nothing left to end.
endButton.onclick = () => {
  if (current) client.end(current).catch((err) => err.code === "not_running" || failed(err));
};
removeButton.onclick = () => {
  if (current) client.remove(current).catch(failed);
};
// Pins the page's scheme; the terminal notices the attribute and reports
// the canvas's scheme and colours again.
schemeSelect.onchange = () => {
  if (schemeSelect.value) document.documentElement.dataset["scheme"] = schemeSelect.value;
  else delete document.documentElement.dataset["scheme"];
};
canvas.focus();
