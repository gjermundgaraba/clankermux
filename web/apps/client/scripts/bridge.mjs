// Development only: serves dist/ over HTTP and bridges /ws (plaintext
// WebSocket, one frame per message) to the server's Unix socket. The TLS
// WebSocket carrier with an Origin allowlist is the server's own (M6).
import { createReadStream, existsSync } from "node:fs";
import { createServer } from "node:http";
import { connect } from "node:net";
import { homedir } from "node:os";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

import { WebSocketServer } from "ws";

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

const HEADER_LEN = 10;

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

const wss = new WebSocketServer({ server: http, path: "/ws" });
wss.on("connection", (ws) => {
  const unix = connect(socket);
  let buffer = Buffer.alloc(0);
  unix.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    let pos = 0;
    while (buffer.length - pos >= HEADER_LEN) {
      const end = pos + HEADER_LEN + buffer.readUInt32LE(pos + 6);
      if (end > buffer.length) break;
      ws.send(buffer.subarray(pos, end));
      pos = end;
    }
    buffer = buffer.subarray(pos);
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
