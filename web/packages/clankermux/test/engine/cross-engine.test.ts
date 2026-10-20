import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { beforeAll, expect, test } from "vitest";

import { ENGINE, Engine } from "../../src/engine/index.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const fixtures = `${here}fixtures/`;
const names = readdirSync(fixtures)
  .filter((f) => f.endsWith(".ghostsnp"))
  .map((f) => f.slice(0, -".ghostsnp".length));
let engine: Engine;
beforeAll(async () => {
  engine = await Engine.load(readFileSync(`${here}../../ghostty-vt.wasm`));
});

test.each(names)("wasm decoder accepts the native encoder's %s snapshot", (name) => {
  const term = engine.decodeSnapshot(readFileSync(`${fixtures}${name}.ghostsnp`));
  expect([term.cols, term.rows]).toEqual([20, 5]);
  expect(term.formatPlain()).toBe(readFileSync(`${fixtures}${name}.txt`, "utf8"));
  if (name === "basic") expect(term.title).toBe("fixture");
  term.free();
});

test("the wasm build reports the engine id", () => {
  engine.scratch(engine.sizeOf("GhosttyString"), (out) => {
    engine.check("ghostty_build_info", engine.enumValue("GhosttyBuildInfo", "VERSION_STRING"), out);
    expect(`ghostty-vt@${engine.readString(out)}`).toBe(ENGINE);
  });
});
