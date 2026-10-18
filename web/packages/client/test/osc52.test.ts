import { expect, test } from "vitest";

import { Osc52Scanner } from "../src/index.ts";

const bytes = (s: string) => new TextEncoder().encode(s);

test("clipboard writes are found across chunks; queries and other OSCs are not", () => {
  const scanner = new Osc52Scanner();
  const hello = btoa("hello");
  expect(scanner.feed(bytes(`text\x1b]52;c;${hello.slice(0, 3)}`))).toEqual([]);
  expect(scanner.feed(bytes(`${hello.slice(3)}\x07more`))).toEqual([{ selection: "c", text: "hello" }]);
  expect(scanner.feed(bytes(`\x1b]52;p;${btoa("x")}\x1b\\`))).toEqual([{ selection: "p", text: "x" }]);
  expect(scanner.feed(bytes("\x1b]52;c;?\x07\x1b]2;title\x07\x1b[31mred"))).toEqual([]);
  // An OSC cut short by another escape is dropped, and scanning resumes.
  expect(scanner.feed(bytes(`\x1b]52;c;${hello}\x1b[0m\x1b]52;c;${btoa("ok")}\x07`))).toEqual([
    { selection: "c", text: "ok" },
  ]);
  expect(scanner.feed(bytes("\x1b]52;c;not base64!\x07"))).toEqual([]);
});
