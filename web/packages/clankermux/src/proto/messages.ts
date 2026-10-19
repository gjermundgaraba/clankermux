export const VERSION = 1;

export interface Peer {
  name: string;
  version: string;
}

export interface Hello {
  version: number;
  client: Peer;
  engine?: string;
  capabilities: string[];
}

export interface Limits {
  max_frame: number;
  control_body: number;
  ring: number;
  input_queue: number;
  scrollback: number;
  continuation: number;
  label: number;
  text: number;
  markers: number;
  records: number;
  page: number;
  hello_timeout: number;
  remove_grace: number;
}

export interface Welcome {
  version: number;
  server: Peer;
  engine: string;
  incarnation: string;
  capabilities: string[];
  limits: Limits;
}

export interface Request {
  id: number;
  method: string;
  params: unknown;
}

export interface Reply {
  id: number;
  result: unknown;
}

export type ErrorCode =
  | "protocol_error"
  | "version_mismatch"
  | "invalid_request"
  | "limit_exceeded"
  | "no_such_session"
  | "not_running"
  | "running"
  | "signal_failed"
  | "engine_mismatch"
  | "channel_in_use"
  | "read_only"
  | "input_overflow"
  | "too_slow"
  | "internal";

export interface ErrorMessage {
  id?: number;
  code: ErrorCode;
  message: string;
}

export type OnExit = "remove" | "keep";
export type Status = "running" | "exited" | "restored";
export type Exit = { code: number } | { signal: string };

/** One program status record (OSC 7501). */
export interface StatusRecord {
  id: string;
  state: string;
  kind?: string;
  app?: string;
  title?: string;
  msg?: string;
  progress?: number;
  updated_at: string;
}

export interface Session {
  id: string;
  label: string;
  status: Status;
  argv: string[];
  cwd: string;
  pid?: number;
  cols: number;
  rows: number;
  title: string;
  pwd: string;
  on_exit: OnExit;
  created_at: string;
  exited_at?: string;
  exit?: Exit;
  offset: number;
  marker: number;
  attachments: number;
  program_status?: StatusRecord[];
}

export interface Page {
  sessions: Session[];
  next?: string;
}

export interface Read {
  text: string;
  cols: number;
  rows: number;
  cursor?: { x: number; y: number };
  offset: number;
  status: Status;
}

export interface ListParams {
  after?: string;
}

export interface SessionParams {
  session: string;
}

export interface SignalParams {
  session: string;
  signal: string;
}

export interface LabelParams {
  session: string;
  label: string;
}

/** `resize`: a side left out keeps the session's. */
export interface ResizeParams {
  session: string;
  cols?: number;
  rows?: number;
}

export interface ReadParams {
  session: string;
  scrollback?: number;
}

export interface SendParams {
  session: string;
  /** Base64. */
  data: string;
}

export interface CreateParams {
  label?: string;
  argv?: string[];
  cwd?: string;
  env?: Record<string, string>;
  cols?: number;
  rows?: number;
  on_exit?: OnExit;
}

export type Event =
  | { event: "session_created"; session: Session }
  | { event: "session_changed"; session: Session }
  | { event: "session_exited"; session: Session }
  | { event: "session_removed"; id: string }
  | { event: "program_status_changed"; id: string; record: StatusRecord }
  | { event: "program_status_removed"; id: string; record_ids: string[] };

export type Scheme = "light" | "dark";

export interface Profile {
  fg?: string;
  bg?: string;
  cursor?: string;
  scheme?: Scheme;
}

/** `OPEN.viewport`: the terminal size in cells and, optionally, pixels. */
export interface Size {
  cols: number;
  rows: number;
  cell_width?: number;
  cell_height?: number;
}

/** The `VIEWPORT` payload. */
export interface Viewport extends Size {
  profile?: Profile;
}

export interface Resume {
  incarnation: string;
  offset: number;
  marker: number;
}

export interface Open {
  session: string;
  read_only?: boolean;
  viewport?: Size;
  profile?: Profile;
  resume?: Resume;
}

export interface Attached {
  mode: "snapshot" | "resume";
  offset: number;
  marker: number;
  cols: number;
  rows: number;
  session: Session;
  /** Total length of the snapshot that follows, in mode `snapshot`. */
  snapshot_bytes?: number;
}

export interface Resized {
  seq: number;
  offset: number;
  cols: number;
  rows: number;
}

export interface Exited {
  seq: number;
  offset: number;
  exit: Exit;
}

export interface Gap {
  from: number;
  to: number;
}

export interface Closed {
  reason: "closed" | "removed" | "stopping";
}
