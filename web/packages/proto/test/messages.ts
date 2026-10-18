// The message vectors as typed literals. The test checks that
// crates/proto/vectors/messages.json holds exactly these values, so a
// field that drifts from the TS types fails `tsc`, and one that drifts
// from the Rust types fails the Rust round trip.

import limits from "../../../../crates/proto/vectors/limits.json";
import type {
  Attached,
  Closed,
  CreateParams,
  ErrorMessage,
  Event,
  Exited,
  Gap,
  Hello,
  ListParams,
  Open,
  Page,
  Read,
  ReadParams,
  Reply,
  Request,
  Resized,
  Session,
  SessionParams,
  SetLabelParams,
  SetSizeParams,
  SignalParams,
  StatusRecord,
  Viewport,
  Welcome,
  WriteParams,
} from "../src/index.ts";

const ENGINE = "ghostty-vt@1.3.2-main+d2865b8";
const SESSION = "0f1e2d3c-0000-4000-8000-000000000001";
const INCARNATION = "7b0a4b1e-5b6a-4c7d-8e9f-0a1b2c3d4e5f";

const running = {
  id: SESSION,
  label: "build",
  status: "running",
  argv: ["/bin/zsh", "-l"],
  cwd: "/Users/me",
  pid: 4242,
  cols: 120,
  rows: 40,
  title: "zsh",
  pwd: "/Users/me/src",
  on_exit: "remove",
  created_at: "2026-10-18T12:00:00Z",
  offset: 1024,
  marker: 2,
  attachments: 1,
} satisfies Session;

const { pid: _pid, ...exitedBase } = running;
const exited = {
  ...exitedBase,
  status: "exited",
  attachments: 0,
  exited_at: "2026-10-18T12:05:00Z",
  exit: { code: 0 },
} satisfies Session;

const blocked = {
  id: "",
  state: "blocked",
  kind: "permission",
  app: "terraform",
  title: "Plan",
  msg: "Apply?",
  progress: 42,
  updated_at: "2026-10-18T12:01:00Z",
} satisfies StatusRecord;

const createParams = {
  label: "build",
  argv: ["/bin/zsh", "-l"],
  cwd: "/Users/me",
  env: { FOO: "bar" },
  cols: 120,
  rows: 40,
  on_exit: "keep",
} satisfies CreateParams;

export const vectors = {
  "hello/full": { version: 1, client: { name: "clankermux", version: "0.1.0" }, engine: ENGINE, capabilities: [] } satisfies Hello,
  "hello/control-only": { version: 1, client: { name: "bot", version: "1" }, capabilities: [] } satisfies Hello,
  "welcome/basic": {
    version: 1,
    server: { name: "clankermux", version: "0.1.0" },
    engine: ENGINE,
    incarnation: INCARNATION,
    capabilities: [],
    limits,
  } satisfies Welcome,
  "request/create": { id: 1, method: "create", params: createParams } satisfies Request,
  "request/list": { id: 2, method: "list", params: { after: SESSION } satisfies ListParams } satisfies Request,
  "request/subscribe": { id: 3, method: "subscribe", params: {} } satisfies Request,
  "reply/empty": { id: 4, result: {} } satisfies Reply,
  "error/request": { id: 5, code: "no_such_session", message: "no session 0f1e" } satisfies ErrorMessage,
  "error/connection": { code: "protocol_error", message: "unknown frame type 0x98" } satisfies ErrorMessage,
  "session/running": running,
  "session/exited": exited,
  "session/inspect": {
    ...running,
    program_status: [blocked, { id: "sync", state: "working", updated_at: "2026-10-18T12:02:00Z" }],
  } satisfies Session,
  "session/signal-exit": { ...exited, exit: { signal: "KILL" } } satisfies Session,
  "page/with-next": { sessions: [running], next: SESSION } satisfies Page,
  "page/last": { sessions: [] } satisfies Page,
  "read/running": { text: "$ ls\nfile\n", cols: 120, rows: 40, cursor: { x: 0, y: 2 }, offset: 1024, status: "running" } satisfies Read,
  "read/restored": { text: "", cols: 80, rows: 24, offset: 0, status: "restored" } satisfies Read,
  "event/session_created": { event: "session_created", session: running } satisfies Event,
  "event/session_removed": { event: "session_removed", id: SESSION } satisfies Event,
  "event/program_status_changed": { event: "program_status_changed", id: SESSION, record: blocked } satisfies Event,
  "event/program_status_removed": { event: "program_status_removed", id: SESSION, record_ids: ["", "sync"] } satisfies Event,
  "params/create": createParams,
  "params/create-empty": {} satisfies CreateParams,
  "params/list": { after: SESSION } satisfies ListParams,
  "params/list-first": {} satisfies ListParams,
  "params/inspect": { session: SESSION } satisfies SessionParams,
  "params/signal": { session: SESSION, signal: "TERM" } satisfies SignalParams,
  "params/set_label": { session: SESSION, label: "deploy" } satisfies SetLabelParams,
  "params/set_size": { session: SESSION, cols: 100, rows: 30 } satisfies SetSizeParams,
  "params/read": { session: SESSION, scrollback: 50 } satisfies ReadParams,
  "params/read-screen": { session: SESSION } satisfies ReadParams,
  "params/write": { session: SESSION, data: "bHMK" } satisfies WriteParams,
  "open/full": {
    session: SESSION,
    read_only: false,
    viewport: { cols: 120, rows: 40, cell_width: 9, cell_height: 18 },
    profile: { fg: "#ffffff", bg: "#000000", cursor: "#00ff00", scheme: "dark" },
    resume: { incarnation: INCARNATION, offset: 1024, marker: 2 },
  } satisfies Open,
  "open/minimal": { session: SESSION, read_only: true } satisfies Open,
  "viewport/with-profile": { cols: 100, rows: 30, profile: { scheme: "light" } } satisfies Viewport,
  "attached/snapshot": { mode: "snapshot", offset: 1024, marker: 2, cols: 120, rows: 40, session: running, snapshot_bytes: 65536 } satisfies Attached,
  "resized/basic": { seq: 3, offset: 2048, cols: 100, rows: 30 } satisfies Resized,
  "exited/code": { seq: 4, offset: 4096, exit: { code: 1 } } satisfies Exited,
  "gap/basic": { from: 100, to: 5000 } satisfies Gap,
  "closed/removed": { reason: "removed" } satisfies Closed,
};
