// Development only: serves dist/ over HTTP, on loopback unless `--host`
// names another address. The page reaches a server through
// `?ws=wss://<node>:<port>/`, or by default the server's port 7438 on the
// page's own host: that default works when the page is served on the
// node's tailnet name (`--host <node>.<tailnet>.ts.net`, the name the
// server's certificate carries), not from loopback. The server must list
// the page's origin in its allowlist (`--origin http://<host>:<port>`).
import { createReadStream, existsSync } from "node:fs";
import { createServer } from "node:http";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : fallback;
};
const port = Number(option("--port", "8787"));
const host = option("--host", "127.0.0.1");
const dist = fileURLToPath(new URL("../dist/", import.meta.url));
const types = { ".html": "text/html", ".js": "text/javascript", ".map": "application/json", ".wasm": "application/wasm" };

createServer((req, res) => {
  const path = normalize(new URL(req.url, "http://x").pathname);
  const file = join(dist, path === "/" ? "index.html" : path);
  if (!file.startsWith(dist) || !existsSync(file)) {
    res.writeHead(404).end();
    return;
  }
  res.writeHead(200, { "content-type": types[extname(file)] ?? "application/octet-stream" });
  createReadStream(file).pipe(res);
}).listen(port, host, () => {
  console.log(`http://${host}:${port}/`);
});
