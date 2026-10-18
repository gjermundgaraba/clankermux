import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";

import { ENGINE, Engine } from "../src/index.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const fixtures = `${here}fixtures/`;
const names = readdirSync(fixtures)
  .filter((f) => f.endsWith(".ghostsnp"))
  .map((f) => f.slice(0, -".ghostsnp".length));

test.each(names)("wasm decoder accepts the native encoder's %s snapshot", async (name) => {
  const engine = await Engine.load(readFileSync(`${here}../ghostty-vt.wasm`));
  const term = engine.decodeSnapshot(readFileSync(`${fixtures}${name}.ghostsnp`));
  expect([term.cols, term.rows]).toEqual([20, 5]);
  expect(term.formatPlain()).toBe(readFileSync(`${fixtures}${name}.txt`, "utf8"));
  if (name === "basic") expect(term.title).toBe("fixture");
  term.free();
});

test("a fresh terminal accepts input", async () => {
  const engine = await Engine.load(readFileSync(`${here}../ghostty-vt.wasm`));
  const term = engine.terminalNew(10, 2);
  term.write(new TextEncoder().encode("hi"));
  expect(term.formatPlain().startsWith("hi")).toBe(true);
  term.free();
});

test("engine id equals the native engine id", () => {
  expect(ENGINE).toBe(readFileSync(`${fixtures}engine.txt`, "utf8"));
});

test("the wasm build reports the engine id", async () => {
  const engine = await Engine.load(readFileSync(`${here}../ghostty-vt.wasm`));
  engine.scratch(engine.sizeOf("GhosttyString"), (out) => {
    engine.check("ghostty_build_info", engine.enumValue("GhosttyBuildInfo", "VERSION_STRING"), out);
    expect(`ghostty-vt@${engine.readString(out)}`).toBe(ENGINE);
  });
});
