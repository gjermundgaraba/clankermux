/**
 * The catalog a real server answers to `describe`, against ajv: every
 * method's examples and every shared vector pass the catalog's JSON
 * Schemas, and a params object with a field the protocol does not define
 * fails them. The TS types are generated from the same catalog.
 */
import { Ajv2020 } from "ajv/dist/2020.js";
import { afterAll, beforeAll, expect, test } from "vitest";

import pkg from "../../package.json" with { type: "json" };
import { type Catalog, VERSION } from "../../src/proto/index.ts";
import { Connection, hello } from "../../src/client/connection.ts";
import { vectors } from "../proto/messages.ts";
import { type Served, StreamTransport, serve } from "./server.ts";

let served: Served;
let conn: Connection;
let catalog: Catalog;

beforeAll(async () => {
  served = await serve();
  conn = await Connection.connect(await StreamTransport.connect(served.socket), hello("test"));
  catalog = await conn.describe();
});

afterAll(async () => {
  conn?.close();
  await served?.stop();
});

/** Validates against one of the catalog's schemas, its `$defs` beside it. */
function validator(schema: unknown): (value: unknown) => boolean {
  // `uint64` and the like are schemars' notes on Rust's integer types.
  const ajv = new Ajv2020({ strict: false, validateFormats: false });
  const root = typeof schema === "boolean" ? schema : { ...(schema as object), $defs: catalog.$defs };
  const validate = ajv.compile(root);
  return (value) => validate(value) as boolean;
}

test("describe answers this protocol and the package's release", async () => {
  expect(catalog.protocol).toBe(VERSION);
  expect(catalog.version).toBe(pkg.version);
  const one = await conn.describe("send");
  expect(one.method).toEqual(catalog.methods.find((m) => m.name === "send"));
  await expect(conn.describe("write")).rejects.toMatchObject({ server: { code: "invalid_request" } });
});

test("every method's examples pass its schemas, and an undefined field fails", () => {
  for (const method of catalog.methods) {
    const params = validator(method.params);
    const result = validator(method.result);
    for (const example of method.examples) {
      expect(params(example.params), `${method.name} params`).toBe(true);
      expect(result(example.result), `${method.name} result`).toBe(true);
      expect(params({ ...(example.params as object), extra: 1 }), `${method.name} strict`).toBe(false);
      expect(result({ ...(example.result as object), extra: 1 }), `${method.name} open`).toBe(true);
    }
  }
});

/** The schema of a shared vector: its frame's, its method's params, or its type's. */
function schemaOf(name: string): unknown {
  // `params/list-first` is `list`'s params.
  const [kind, variant] = name.split("/") as [string, string];
  if (kind === "params") return catalog.methods.find((m) => m.name === variant.split("-")[0])?.params;
  const frame = catalog.frames.find((f) => f.name.toLowerCase() === kind);
  if (frame) return frame.schema;
  // `session/running` is a `Session`.
  const type = kind.charAt(0).toUpperCase() + kind.slice(1);
  return Object.hasOwn(catalog.$defs, type) ? { $ref: `#/$defs/${type}` } : undefined;
}

test("every shared vector passes the schema of its frame, method or type", () => {
  for (const [name, value] of Object.entries(vectors)) {
    const schema = schemaOf(name);
    expect(schema, `${name} has a schema`).toBeTruthy();
    expect(validator(schema)(value), name).toBe(true);
  }
  for (const event of catalog.events) {
    const schema = validator(event.schema);
    for (const example of event.examples) {
      const { seq: _seq, ...body } = example as { seq: number };
      expect(schema(body), event.name).toBe(true);
    }
  }
});
