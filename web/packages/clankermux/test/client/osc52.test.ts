import { expect, test } from "vitest";

import { Osc52Scanner } from "../../src/client/osc52.ts";

const bytes = (s: string) => new TextEncoder().encode(s);

test("clipboard writes are found across chunks; queries and other OSCs are not", () => {
  const scanner = new Osc52Scanner();
  const hello = btoa("hello");
  expect(scanner.feed(bytes(`text\x1b]52;c;${hello.slice(0, 3)}`))).toEqual([]);
  expect(scanner.feed(bytes(`${hello.slice(3)}\x07more`))).toEqual([{ toClipboard: true, text: "hello" }]);
  expect(scanner.feed(bytes(`\x1b]52;p;${btoa("x")}\x1b\\`))).toEqual([{ toClipboard: false, text: "x" }]);
  expect(scanner.feed(bytes("\x1b]52;c;?\x07\x1b]2;title\x07\x1b[31mred"))).toEqual([]);
  expect(scanner.feed(bytes(`\x1b]52;c;${hello}\x1b[0m\x1b]52;c;${btoa("ok")}\x07`))).toEqual([
    { toClipboard: true, text: "hello" },
    { toClipboard: true, text: "ok" },
  ]);
  expect(scanner.feed(bytes("\x1b]52;c;not base64!\x07"))).toEqual([]);
  expect(scanner.feed(bytes(`\x1b]52;c;${hello}\x1b]52;c;${btoa("two")}\x07`))).toEqual([
    { toClipboard: true, text: "hello" },
    { toClipboard: true, text: "two" },
  ]);
  expect(scanner.feed(bytes(`\x1b]52;c;${hello}\x18\x1b]52;c;${hello}\x1a\x07`))).toEqual([]);
  expect(scanner.feed(bytes("\x1b]52;c;QUJD"))).toEqual([]);
  scanner.reset();
  expect(scanner.feed(bytes("\x07plain"))).toEqual([]);
  expect(scanner.feed(bytes(`\x1b]8;;${"x".repeat(3000)}\x1b\\\x1b]52;;${btoa("k")}\x07`))).toEqual([
    { toClipboard: true, text: "k" },
  ]);
});

test("text skipping preserves OSC state at every chunk boundary", () => {
  const stream = bytes(
    "plain".repeat(100) + "\x1b\x1b]52;c;YQ==\x07" +
    "text\x1b]5x;ignored\x07\x1b]52x;ignored\x1b\\" +
    "\x1b]52;c;Yg==\x18\x1b]52;c;Yw==\x1a" +
    "tail\x1b]52;;ZA==\x1b]52;p;ZQ==\x1b\\end",
  );
  const expected = [
    { toClipboard: true, text: "a" },
    { toClipboard: true, text: "d" },
    { toClipboard: false, text: "e" },
  ];
  for (let split = 0; split <= stream.length; split++) {
    const scanner = new Osc52Scanner();
    expect([...scanner.feed(stream.subarray(0, split)), ...scanner.feed(stream.subarray(split))]).toEqual(expected);
  }
  const scanner = new Osc52Scanner();
  expect(Array.from(stream).flatMap((b) => scanner.feed(Uint8Array.of(b)))).toEqual(expected);
});

test("long clipboard payloads keep their prefix and oversized ones recover at the next OSC", () => {
  const scanner = new Osc52Scanner();
  const text = "clipboard".repeat(8192);
  const encoded = bytes(`\x1b]52;c;${btoa(text)}\x07`);
  expect(scanner.feed(encoded.subarray(0, 6))).toEqual([]);
  expect(scanner.feed(encoded.subarray(6))).toEqual([{ toClipboard: true, text }]);
  expect(scanner.feed(bytes(`\x1b]52;c;${"A".repeat(1 << 20)}\x07plain\x1b]52;c;eA==\x07`))).toEqual([
    { toClipboard: true, text: "x" },
  ]);
});
