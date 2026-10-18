// Development only: serves dist/ over HTTP on loopback. The page then
// reaches a server through `?ws=wss://<host>:<port>/`; the server must
// list this origin in its allowlist (`--origin http://127.0.0.1:<port>`).
import { createReadStream, existsSync } from "node:fs";
import { createServer } from "node:http";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const i = args.indexOf("--port");
const port = Number(i >= 0 ? args[i + 1] : "8787");
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
}).listen(port, "127.0.0.1", () => {
  console.log(`http://127.0.0.1:${port}/`);
});
