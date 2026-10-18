// Drives the page in headless Chrome over the DevTools protocol: waits for
// the attach, types a line, screenshots. A development check, not a test.
//
//   node scripts/headless.mjs <page url> <screenshot.png> [text to type]
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";

const [url, shot, typed = "echo browser-ok\n"] = process.argv.slice(2);
if (!url || !shot) {
  console.error("usage: headless.mjs <url> <screenshot.png> [text]");
  process.exit(2);
}
const chrome = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const proc = spawn(chrome, [
  "--headless=new",
  "--no-first-run",
  "--disable-gpu",
  `--user-data-dir=/tmp/clankermux-headless-${process.pid}`,
  "--window-size=900,500",
  "--remote-debugging-port=0",
  "about:blank",
]);
const endpoint = await new Promise((resolve, reject) => {
  let err = "";
  proc.stderr.on("data", (chunk) => {
    err += chunk;
    const m = /DevTools listening on (ws:\/\/\S+)/.exec(err);
    if (m) resolve(m[1]);
  });
  proc.on("exit", () => reject(new Error(`chrome exited: ${err}`)));
});

const ws = new WebSocket(endpoint);
await new Promise((r) => (ws.onopen = r));
let nextId = 1;
const waiting = new Map();
const logs = [];
const workers = new Set();
let pageSession;
ws.onmessage = (e) => {
  const msg = JSON.parse(e.data);
  if (msg.id !== undefined) {
    const { resolve, reject } = waiting.get(msg.id);
    waiting.delete(msg.id);
    msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
    return;
  }
  const where = workers.has(msg.sessionId) ? "worker" : "page";
  if (msg.method === "Runtime.consoleAPICalled") {
    logs.push(`${where} console.${msg.params.type}: ${msg.params.args.map((a) => a.value ?? a.description).join(" ")}`);
  } else if (msg.method === "Runtime.exceptionThrown") {
    const d = msg.params.exceptionDetails;
    logs.push(`${where} exception: ${d.exception?.description ?? d.text}`);
  } else if (msg.method === "Target.attachedToTarget") {
    const { sessionId, targetInfo } = msg.params;
    if (targetInfo.type === "worker") {
      workers.add(sessionId);
      void send("Runtime.enable", {}, sessionId);
      void send("Runtime.runIfWaitingForDebugger", {}, sessionId);
    }
  }
};
function send(method, params = {}, sessionId = pageSession) {
  const id = nextId++;
  ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
  return new Promise((resolve, reject) => waiting.set(id, { resolve, reject }));
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const evaluate = async (expression) => (await send("Runtime.evaluate", { expression, returnByValue: true })).result.value;

try {
  const { targetId } = await send("Target.createTarget", { url: "about:blank" }, undefined);
  pageSession = (await send("Target.attachToTarget", { targetId, flatten: true }, undefined)).sessionId;
  await send("Runtime.enable");
  await send("Page.enable");
  await send("Target.setAutoAttach", { autoAttach: true, waitForDebuggerOnStart: true, flatten: true });
  await send("Emulation.setFocusEmulationEnabled", { enabled: true });
  await send("Page.navigate", { url });
  let status = "";
  for (let i = 0; i < 100; i++) {
    status = await evaluate("document.getElementById('status').textContent");
    if (status === "attached") break;
    await sleep(100);
  }
  console.log(`status: ${status}`);
  if (status === "attached") {
    await evaluate("document.getElementById('screen').focus()");
    for (const ch of typed) {
      const key = ch === "\n" ? "Enter" : ch;
      const code = ch === "\n" ? "Enter" : ch === " " ? "Space" : /[a-z]/i.test(ch) ? `Key${ch.toUpperCase()}` : /[0-9]/.test(ch) ? `Digit${ch}` : "Unidentified";
      await send("Input.dispatchKeyEvent", { type: "keyDown", key, code, text: ch === "\n" ? "\r" : ch });
      await send("Input.dispatchKeyEvent", { type: "keyUp", key, code });
    }
    await sleep(700);
  }
  const { data } = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(shot, Buffer.from(data, "base64"));
  console.log(`screenshot: ${shot}`);
} finally {
  for (const line of logs) console.log(line);
  proc.kill();
}
