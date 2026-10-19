// Drives the page in headless Chrome over the DevTools protocol: waits for
// the attach, types a line, screenshots, and checks the typed text reached
// the session's screen through the server's own `read`. Exits non-zero
// when any step fails. A development check; it types plain ASCII only, so
// it says nothing about modifiers or layouts.
//
//   node scripts/headless.mjs <page url> <socket> <session id> <screenshot.png>
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
import { connect } from "node:net";

import { FrameType, StreamFramer, encodeFrame, parseFrame } from "../../../packages/clankermux/src/proto/index.ts";

const [url, socket, session, shot] = process.argv.slice(2);
if (!url || !socket || !session || !shot) {
  console.error("usage: headless.mjs <url> <socket> <session id> <screenshot.png>");
  process.exit(2);
}
const marker = `browser-ok-${process.pid}`;
const typed = `echo ${marker}\n`;

/** The session's screen text, through the control plane. */
function readScreen() {
  return new Promise((resolve, reject) => {
    const unix = connect(socket);
    const framer = new StreamFramer(1 << 30);
    const json = (type, body) => unix.write(encodeFrame(type, 0, new TextEncoder().encode(JSON.stringify(body))));
    unix.on("connect", () => json(FrameType.Hello, { version: 1, client: { name: "headless", version: "0" }, capabilities: [] }));
    unix.on("data", (chunk) => {
      for (const bytes of framer.feed(chunk)) {
        const frame = parseFrame(bytes, 1 << 30);
        const body = JSON.parse(new TextDecoder().decode(frame.payload));
        if (frame.header.type === FrameType.Welcome) json(FrameType.Request, { id: 1, method: "read", params: { session } });
        else if (frame.header.type === FrameType.Reply) {
          unix.destroy();
          resolve(body.result.text);
        } else if (frame.header.type === FrameType.Error) {
          unix.destroy();
          reject(new Error(`${body.code}: ${body.message}`));
        }
      }
    });
    unix.on("error", reject);
  });
}
const chrome = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const proc = spawn(chrome, [
  "--headless=new",
  "--no-first-run",
  // A loopback check runs on a self-signed certificate.
  "--ignore-certificate-errors",
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
  let ok = status === "attached";
  if (ok) {
    await evaluate("document.getElementById('screen').focus()");
    for (const ch of typed) {
      const key = ch === "\n" ? "Enter" : ch;
      const code = ch === "\n" ? "Enter" : ch === " " ? "Space" : /[a-z]/i.test(ch) ? `Key${ch.toUpperCase()}` : /[0-9]/.test(ch) ? `Digit${ch}` : "Unidentified";
      await send("Input.dispatchKeyEvent", { type: "keyDown", key, code, text: ch === "\n" ? "\r" : ch });
      await send("Input.dispatchKeyEvent", { type: "keyUp", key, code });
    }
    await sleep(700);
    const screen = await readScreen();
    // The echo appears once as the command line and once as output.
    ok = screen.split(marker).length >= 3;
    console.log(ok ? `typed text reached the session` : `typed text missing from the screen:\n${screen}`);
  }
  const { data } = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(shot, Buffer.from(data, "base64"));
  console.log(`screenshot: ${shot}`);
  process.exitCode = ok ? 0 : 1;
} finally {
  for (const line of logs) console.log(line);
  proc.kill();
}
