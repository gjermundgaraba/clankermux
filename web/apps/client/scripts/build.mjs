// Bundles the page and its worker into dist/, with the wasm and the page.
import { copyFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";

import * as esbuild from "esbuild";

const root = fileURLToPath(new URL("..", import.meta.url));
const dist = `${root}dist/`;
mkdirSync(dist, { recursive: true });
copyFileSync(`${root}index.html`, `${dist}index.html`);
copyFileSync(fileURLToPath(import.meta.resolve("@gjermundgaraba/clankermux/ghostty-vt.wasm")), `${dist}ghostty-vt.wasm`);

const options = {
  entryPoints: [`${root}src/main.ts`, `${root}src/worker.ts`],
  bundle: true,
  format: "esm",
  target: "es2022",
  outdir: dist,
  sourcemap: true,
  logLevel: "info",
};

if (process.argv.includes("--watch")) {
  const context = await esbuild.context(options);
  await context.watch();
} else {
  await esbuild.build(options);
}
