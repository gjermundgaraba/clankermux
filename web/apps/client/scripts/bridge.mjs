// Development only: serves dist/ over HTTP and bridges /ws (plaintext
// WebSocket, one frame per message) to the server's Unix socket. Only the
// page it serves may connect: any other origin is refused, as the server's
// own TLS WebSocket carrier (M6) will refuse origins outside its allowlist.
import { createReadStream, existsSync } from "node:fs";
import { createServer } from "node:http";
import { connect } from "node:net";
import { homedir } from "node:os";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

import { WebSocketServer } from "ws";

import { StreamFramer } from "../../../packages/proto/src/stream.ts";

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] !== undefined ? args[i + 1] : fallback;
};
const port = Number(option("--port", "8787"));
const socket = option(
  "--socket",
  process.env.XDG_RUNTIME_DIR
    ? join(process.env.XDG_RUNTIME_DIR, "clankermux/sock")
    : join(process.env.XDG_STATE_HOME ?? join(homedir(), ".local/state"), "clankermux/sock"),
);
const dist = fileURLToPath(new URL("../dist/", import.meta.url));
const types = { ".html": "text/html", ".js": "text/javascript", ".map": "application/json", ".wasm": "application/wasm" };

const http = createServer((req, res) => {
  const path = normalize(new URL(req.url, "http://x").pathname);
  const file = join(dist, path === "/" ? "index.html" : path);
  if (!file.startsWith(dist) || !existsSync(file)) {
    res.writeHead(404).end();
    return;
  }
  res.writeHead(200, { "content-type": types[extname(file)] ?? "application/octet-stream" });
  createReadStream(file).pipe(res);
});

const origins = new Set([`http://127.0.0.1:${port}`, `http://localhost:${port}`]);
const wss = new WebSocketServer({
  server: http,
  path: "/ws",
  verifyClient: ({ origin }) => origins.has(origin ?? ""),
});
wss.on("connection", (ws) => {
  const unix = connect(socket);
  const framer = new StreamFramer(1 << 30);
  unix.on("data", (chunk) => {
    for (const frame of framer.feed(chunk)) ws.send(frame);
  });
  unix.on("close", () => ws.close());
  unix.on("error", (err) => {
    console.error(`unix socket: ${err.message}`);
    ws.close();
  });
  ws.on("message", (data) => unix.write(data));
  ws.on("close", () => unix.destroy());
});

http.listen(port, "127.0.0.1", () => {
  console.log(`http://127.0.0.1:${port}/  ->  ${socket}`);
});
