/** What a server sends, for the suites that play one. */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import type { Limits, Session, Welcome } from "../../src/proto/index.ts";

const here = fileURLToPath(new URL(".", import.meta.url));
export const limits: Limits = JSON.parse(readFileSync(`${here}../../../../../crates/proto/vectors/limits.json`, "utf8"));

export function welcome(incarnation: string): Welcome {
  return {
    version: 1,
    server: { name: "test", version: "0" },
    engine: "x",
    incarnation,
    capabilities: [],
    limits,
  };
}

/** A 10×3 session, running unless `fields` say otherwise. */
export function session(id: string, fields: Partial<Session> = {}): Session {
  return {
    id,
    label: "",
    status: "running",
    argv: [],
    cwd: "",
    cols: 10,
    rows: 3,
    title: "",
    pwd: "",
    on_exit: "keep",
    created_at: "",
    offset: 0,
    marker: 0,
    attachments: [],
    size: { policy: "latest" },
    ...fields,
  };
}

/** An `OUTPUT` payload: `text` at stream `offset`. */
export function output(offset: number, text: string): Uint8Array {
  const bytes = new TextEncoder().encode(text);
  const payload = new Uint8Array(8 + bytes.length);
  new DataView(payload.buffer).setBigUint64(0, BigInt(offset), true);
  payload.set(bytes, 8);
  return payload;
}
