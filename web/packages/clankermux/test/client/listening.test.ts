import { PassThrough } from "node:stream";
import { expect, test } from "vitest";

import { listeningOn } from "./listening.ts";

const delivered = () => new Promise((resolve) => setImmediate(resolve));

test("the address is read from the whole line when the server's writes split it", async () => {
  const stderr = new PassThrough();
  const url = listeningOn(stderr);
  stderr.write("listening on /tmp/cmx-x/sock\nlistening on wss://127.0");
  await delivered();
  stderr.write(".0.1:49488\n");
  await expect(url).resolves.toBe("wss://127.0.0.1:49488");
});

test("a server that ends its stderr without listening fails the wait", async () => {
  const stderr = new PassThrough();
  const url = listeningOn(stderr);
  stderr.end("error: address in use\n");
  await expect(url).rejects.toThrow(/exited before it listened/);
});
