import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";

import { ENGINE, Engine } from "../src/index.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const fixtures = `${here}fixtures/`;

test("wasm decoder accepts the native encoder's snapshot", async () => {
  const engine = await Engine.load(readFileSync(`${here}../ghostty-vt.wasm`));
  const term = engine.decodeSnapshot(readFileSync(`${fixtures}basic.ghostsnp`));
  expect([term.cols, term.rows]).toEqual([20, 5]);
  expect(term.title).toBe("fixture");
  expect(term.formatPlain()).toBe(readFileSync(`${fixtures}basic.txt`, "utf8"));
  term.free();
});

test("engine id names the vendored build", () => {
  expect(ENGINE).toMatch(/^ghostty-vt@/);
});
