import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { beforeAll, expect, test } from "vitest";

import { Decoder, ENGINE, Engine, type Terminal } from "../../src/engine/index.ts";

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

test.each(names)("wasm decoder streams the native encoder's %s snapshot as small pieces arrive", (name) => {
  const bytes = readFileSync(`${fixtures}${name}.ghostsnp`);
  for (const piece of [1, 7]) {
    const decoder = new Decoder(engine, bytes.length);
    decoder.setMaxContinuationBytes(1 << 20);
    let term: Terminal | undefined;
    for (let pos = 0; pos < bytes.length; pos += piece) {
      decoder.push(bytes.subarray(pos, pos + piece));
      if (!term && decoder.readyAvailable()) term = decoder.ready();
      while (term && decoder.stepAvailable()) decoder.nextPage();
    }
    expect(decoder.finished(), `${piece}-byte pieces`).toBe(true);
    const restored = decoder.release()!;
    expect(restored.formatPlain()).toBe(readFileSync(`${fixtures}${name}.txt`, "utf8"));
    restored.free();
  }
});

test("the wasm build reports the engine id", () => {
  engine.scratch(engine.sizeOf("GhosttyString"), (out) => {
    engine.check("ghostty_build_info", engine.enumValue("GhosttyBuildInfo", "VERSION_STRING"), out);
    expect(`ghostty-vt@${engine.readString(out)}`).toBe(ENGINE);
  });
});
