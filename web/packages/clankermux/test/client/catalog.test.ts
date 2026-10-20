/**
 * The catalog a real server answers to `describe` against the TS types:
 * every method's example, typed here by its `Methods` entry, and every
 * shared vector pass the catalog's JSON Schemas, and a params object with
 * a field the protocol does not define fails them.
 */
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { connect as netConnect, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Ajv2020 } from "ajv/dist/2020.js";
import { afterAll, beforeAll, expect, test } from "vitest";

import pkg from "../../package.json" with { type: "json" };
import { type Catalog, type JsonSchema, type Method, type Methods, StreamFramer, VERSION } from "../../src/proto/index.ts";
import { Connection, type Transport, hello } from "../../src/client/index.ts";
import { vectors } from "../proto/messages.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
const binary = `${here}../../../../../target/debug/clankermux`;
if (!existsSync(binary)) throw new Error(`${binary} is missing: run cargo build first`);

class StreamTransport implements Transport {
  private readonly framer = new StreamFramer(1 << 30);
  onMessage: ((bytes: Uint8Array) => void) | undefined;
  onClose: (() => void) | undefined;

  constructor(private readonly socket: Socket) {
    socket.on("data", (chunk: Buffer) => {
      for (const frame of this.framer.feed(chunk)) this.onMessage?.(frame);
    });
    socket.on("close", () => this.onClose?.());
    socket.on("error", () => {});
  }

  send(bytes: Uint8Array): void {
    this.socket.write(bytes);
  }

  close(): void {
    this.socket.destroy();
  }
}

function dial(path: string): Promise<StreamTransport> {
  return new Promise((resolve, reject) => {
    const socket = netConnect(path);
    socket.once("connect", () => resolve(new StreamTransport(socket)));
    socket.once("error", reject);
  });
}

let dir: string;
let server: ReturnType<typeof spawn>;
let conn: Connection;
let catalog: Catalog;

beforeAll(async () => {
  dir = mkdtempSync(join(tmpdir(), "cmx-"));
  const socket = join(dir, "sock");
  server = spawn(binary, ["--socket", socket, "serve"], {
    stdio: "ignore",
    env: { ...process.env, XDG_STATE_HOME: join(dir, "state") },
  });
  const deadline = Date.now() + 10_000;
  for (;;) {
    try {
      conn = await Connection.connect(await dial(socket), hello("test"));
      break;
    } catch {
      if (Date.now() > deadline) throw new Error("server did not listen");
      await new Promise((r) => setTimeout(r, 20));
    }
  }
  catalog = await conn.describe();
});

afterAll(async () => {
  conn?.close();
  if (server && server.exitCode === null) await new Promise((resolve) => (server.once("exit", resolve), server.kill()));
  if (dir) rmSync(dir, { recursive: true, force: true });
});

/** Validates against one of the catalog's schemas, its `$defs` beside it. */
function validator(schema: JsonSchema): (value: unknown) => boolean {
  // `uint64` and the like are schemars' notes on Rust's integer types.
  const ajv = new Ajv2020({ strict: false, validateFormats: false });
  const root = typeof schema === "boolean" ? schema : { ...schema, $defs: catalog.$defs };
  const validate = ajv.compile(root);
  return (value) => validate(value) as boolean;
}

type Examples = { [M in Method]: { params: Methods[M]["params"]; result: Methods[M]["result"] }[] };

/** Each method's examples, as the catalog lists them, through the TS types. */
async function examples(): Promise<Examples> {
  return {
    create: [{ params: vectors["params/create"], result: vectors["session/running"] }],
    list: [
      { params: vectors["params/list-first"], result: vectors["page/with-next"] },
      { params: vectors["params/list"], result: vectors["page/last"] },
    ],
    inspect: [{ params: vectors["params/inspect"], result: vectors["session/inspect"] }],
    end: [{ params: vectors["params/end"], result: vectors["empty/basic"] }],
    signal: [{ params: vectors["params/signal"], result: vectors["empty/basic"] }],
    remove: [{ params: vectors["params/remove"], result: vectors["empty/basic"] }],
    label: [{ params: vectors["params/label"], result: vectors["session/running"] }],
    resize: [
      { params: vectors["params/resize"], result: vectors["empty/basic"] },
      { params: vectors["params/resize-policy"], result: vectors["empty/basic"] },
    ],
    read: [
      { params: vectors["params/read"], result: vectors["read/running"] },
      { params: vectors["params/read-scoped"], result: vectors["read/truncated"] },
    ],
    tail: [
      { params: vectors["params/tail"], result: vectors["tail/basic"] },
      { params: vectors["params/tail-from"], result: vectors["tail/truncated"] },
    ],
    send: [
      { params: vectors["params/send"], result: vectors["sent/plain"] },
      { params: vectors["params/send-text"], result: vectors["sent/wait"] },
    ],
    wait: [
      { params: vectors["params/wait"], result: vectors["waited/status"] },
      { params: vectors["params/wait-output"], result: vectors["waited/timeout"] },
    ],
    cancel: [{ params: vectors["params/cancel"], result: vectors["cancelled/yes"] }],
    detach: [{ params: vectors["params/detach"], result: vectors["empty/basic"] }],
    subscribe: [
      { params: vectors["params/subscribe"], result: vectors["subscribed/first"] },
      { params: vectors["params/subscribe-after"], result: vectors["subscribed/after"] },
    ],
    ack: [{ params: vectors["params/ack"], result: vectors["acked/basic"] }],
    describe: [{ params: vectors["params/describe-method"], result: await conn.describe("cancel") }],
  };
}

test("describe answers this protocol and the package's release", async () => {
  expect(catalog.protocol).toBe(VERSION);
  expect(catalog.version).toBe(pkg.version);
  const one = await conn.describe("send");
  expect(one.method).toEqual(catalog.methods.find((m) => m.name === "send"));
  await expect(conn.describe("write")).rejects.toMatchObject({ server: { code: "invalid_request" } });
});

test("every method's typed examples pass its schemas, and an undefined field fails", async () => {
  const typed = await examples();
  expect(catalog.methods.map((m) => m.name)).toEqual(Object.keys(typed));
  for (const method of catalog.methods) {
    const mine = typed[method.name as Method];
    expect(mine, method.name).toEqual(method.examples);
    const params = validator(method.params);
    const result = validator(method.result);
    for (const example of mine) {
      expect(params(example.params), `${method.name} params`).toBe(true);
      expect(result(example.result), `${method.name} result`).toBe(true);
      expect(params({ ...example.params, extra: 1 }), `${method.name} strict`).toBe(false);
      expect(result({ ...(example.result as object), extra: 1 }), `${method.name} open`).toBe(true);
    }
  }
});

test("every shared vector passes the schema of its frame, method or type", () => {
  const frames = new Map(catalog.frames.map((f) => [f.name.toLowerCase(), f.schema]));
  const types: Record<string, string> = {
    session: "Session",
    page: "Page",
    subscribed: "Subscribed",
    read: "Read",
    tail: "Tail",
    sent: "Sent",
    waited: "Waited",
    cancelled: "Cancelled",
    acked: "Acked",
    empty: "Empty",
  };
  for (const [name, value] of Object.entries(vectors)) {
    // `params/list-first` is `list`'s params.
    const [kind, variant] = name.split("/") as [string, string];
    const schema =
      kind === "params"
        ? catalog.methods.find((m) => m.name === variant.split("-")[0])?.params
        : (frames.get(kind) ?? (types[kind] === undefined ? undefined : { $ref: `#/$defs/${types[kind]}` }));
    expect(schema, `${name} has a schema`).toBeTruthy();
    expect(validator(schema!)(value), name).toBe(true);
  }
  for (const event of catalog.events) {
    const schema = validator(event.schema);
    for (const example of event.examples) {
      const { seq: _seq, ...body } = example as { seq: number };
      expect(schema(body), event.name).toBe(true);
    }
  }
});
