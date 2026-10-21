// Generated from the catalog by `cargo xtask protocol`; do not edit.

export const VERSION = 1;

/** Each method's params and result. */
export interface Methods {
  /** Starts a session: a program on a new PTY. */
  create: { params: CreateParams; result: Session };
  /** A page of sessions, ordered by id, without their records. */
  list: { params: ListParams; result: Page };
  /** One session, with its program status records. */
  inspect: { params: SessionParams; result: Session };
  /** Sends SIGHUP to the session's processes. */
  end: { params: SessionParams; result: Empty };
  /** Sends a signal, by name, to the session's processes. */
  signal: { params: SignalParams; result: Empty };
  /** Deletes a session that is not running, with its files. */
  remove: { params: SessionParams; result: Empty };
  /** Sets the session's label. */
  label: { params: LabelParams; result: Session };
  /** Sets the session's size, its size policy, or both. */
  resize: { params: ResizeParams; result: Empty };
  /** The session's screen, history or last command, as text, VT or HTML. */
  read: { params: ReadParams; result: Read };
  /** The output stream as plain text, from a stream offset. */
  tail: { params: TailParams; result: Tail };
  /**
   * Writes text, named keys or bytes to the PTY; with `wait`, answers when it
   * ends.
   */
  send: { params: SendParams; result: Sent };
  /**
   * Answers when the session exits, its output matches, a record is in a state,
   * a prompt shows or output stops.
   */
  wait: { params: WaitParams; result: Waited };
  /** Ends a pending `wait`, or `send` with `wait`, of this connection. */
  cancel: { params: CancelParams; result: Cancelled };
  /** Ends an attachment of any connection. */
  detach: { params: DetachParams; result: Empty };
  /**
   * Starts `EVENT` frames on this connection: from now, or replayed after a
   * `seq`.
   */
  subscribe: { params: SubscribeParams; result: Subscribed };
  /** Marks `done` and `error` records seen. */
  ack: { params: AckParams; result: Acked };
  /** This catalog, or one method of it. */
  describe: { params: DescribeParams; result: Described };
}

export type Method = keyof Methods;

/** `ack`: every `done` and `error` record when `record_ids` is absent. */
export interface AckParams {
  /** The session's id. */
  session: string;
  /** Default: every `done` and `error` record. */
  record_ids?: string[];
}

/** The `ack` reply: the records it marked seen. */
export interface Acked {
  record_ids: string[];
}

export type AttachMode = "snapshot" | "resume";

export interface Attached {
  mode: AttachMode;
  /** Cursor offset the attachment continues from. */
  offset: number;
  /**
   * Cursor marker: latest `seq` reflected in the snapshot, or in the client's
   * terminal when resuming.
   */
  marker: number;
  cols: number;
  rows: number;
  session: Session;
  /**
   * Required in mode `snapshot`, absent otherwise: the total length of the
   * snapshot that follows (the sum of the `SNAPSHOT` payloads), so a client can
   * size its buffer.
   */
  snapshot_bytes?: number;
}

/** One open attachment of a session, as `list` and `inspect` show it. */
export interface AttachmentInfo {
  /**
   * Assigned by the server at `OPEN`, unique on it and never reused while it
   * runs; not the frame channel, which only its connection knows. `detach`
   * takes it.
   */
  channel: number;
  /**
   * From the connection's `HELLO.client`, each cut to 32 bytes with control
   * characters replaced by `?`.
   */
  client: Peer;
  read_only: boolean;
  /**
   * Its latest viewport report, in `OPEN` or `VIEWPORT`; absent until it makes
   * one.
   */
  viewport?: Cells;
  /** When it opened, RFC 3339. */
  since: string;
  /** When it last sent `INPUT` the server accepted, RFC 3339. */
  last_input?: string;
}

/** `Session.attention`: the most urgent record that wants the user. */
export interface Attention {
  state: string;
  kind?: string;
  app?: string;
  msg?: string;
  /** The record's id. */
  record: string;
}

export interface CancelParams {
  /** The pending request's id on this connection. */
  id: number;
}

/** The `cancel` result. */
export interface Cancelled {
  cancelled: boolean;
}

/** The `describe {}` result. */
export interface Catalog {
  /** The protocol version, as in `WELCOME`. */
  protocol: number;
  /** The server's release. */
  version: string;
  methods: MethodInfo[];
  events: EventInfo[];
  frames: FrameInfo[];
  errors: ErrorInfo[];
  /** The schemas the others refer to, as `#/$defs/NAME`. */
  $defs: Record<string, unknown>;
}

/** A size in cells. */
export interface Cells {
  cols: number;
  rows: number;
}

/** What a method does to the server's state. */
export type Class =
  /** Answers from the server's state and changes nothing. */
  | "read"
  /** Changes a session, an attachment or a pending request. */
  | "mutation"
  /** Starts frames that follow the reply on the connection. */
  | "stream";

/** `HELLO.client`: a `Peer` as the client sends it. */
export interface ClientPeer {
  name: string;
  version: string;
}

export interface Closed {
  reason: ClosedReason;
  /** The `reason` a `detach` gave. */
  message?: string;
}

export type ClosedReason = "closed" | "removed" | "stopping" | "detached";

/**
 * One command from its OSC 133 marks, as stream offsets: `start` at the prompt
 * (`A`), `output` where its output begins (`C`), `end` where it ended (`D`)
 * with the `exit` the shell reported.
 */
export interface Command {
  start: number;
  output?: number;
  end?: number;
  exit?: number;
}

export interface CreateParams {
  /** At most `limits.label` bytes. */
  label?: string;
  /** Default: the user's login shell. */
  argv?: string[];
  /** Default: the server user's home directory. */
  cwd?: string;
  /** Additions and overrides of the server's environment. */
  env?: Record<string, string>;
  /** Default 80. */
  cols?: number;
  /** Default 24. */
  rows?: number;
  /** Default `remove`. */
  on_exit?: OnExit;
}

export interface Cursor {
  x: number;
  y: number;
}

/** A stream cursor as a request gives it. */
export interface CursorParams {
  offset: number;
  marker: number;
}

/** `describe`: one method, or the whole catalog. */
export interface DescribeParams {
  /** A method's name; absent: every method. */
  method?: string;
}

/** The `describe` result: `Description` with `method`, else `Catalog`. */
export type Described = Description | Catalog;

/** The `describe {method}` result: the method and the schemas it refers to. */
export interface Description {
  /** The protocol version, as in `WELCOME`. */
  protocol: number;
  /** The server's release. */
  version: string;
  method: MethodInfo;
  /** The schemas `method` refers to, as `#/$defs/NAME`. */
  $defs: Record<string, unknown>;
}

/** `detach`: `reason` goes to the attachment as `CLOSED.message`. */
export interface DetachParams {
  /** The session's id. */
  session: string;
  /** The attachment's `channel`, as `inspect` lists it. */
  channel: number;
  /** Told to the attachment; at most `limits.text` bytes. */
  reason?: string;
}

/** Which side sends a frame. */
export type Direction = "client" | "server";

/** The result of a method that answers with success alone. */
export type Empty = Record<string, unknown>;

export type ErrorCode =
  /** A connection error under Framing; the connection closes. */
  | "protocol_error"
  /** `HELLO.version` is not 1; the connection closes. */
  | "version_mismatch"
  /**
   * Malformed payload, a field the protocol does not define, unknown method, a
   * resume marker above the session's latest, a `subscribe` `after` above the
   * latest event, `read` with `scrollback` and a scope other than `screen`, or
   * `tail` from past the stream's end or with `max_bytes` 0.
   */
  | "invalid_request"
  /** `label`, `text` or `attachments` exceeded, or a snapshot failed. */
  | "limit_exceeded"
  /** No session has the id. */
  | "no_such_session"
  /** `detach` named no open attachment of the session. */
  | "no_such_attachment"
  /**
   * `status` is not `running`, or the stream has ended; for a wait, nothing
   * matched before the stream ended.
   */
  | "not_running"
  /** `remove` on a running session. */
  | "running"
  /** `end` or `signal` found no process to signal. */
  | "signal_failed"
  /** Engine ids differ. */
  | "engine_mismatch"
  /** `OPEN` on a channel already in use. */
  | "channel_in_use"
  /** `INPUT` or `VIEWPORT` on a read-only attachment. */
  | "read_only"
  /** The input would exceed `limits.input_queue`. */
  | "input_overflow"
  /** Second gap during a bootstrap. */
  | "too_slow"
  /** A `cancel` ended the request. */
  | "cancelled"
  /**
   * `read` of `alternate` off the alternate screen, or of `last_command` with
   * no command output marked.
   */
  | "scope_unavailable"
  /** The server failed, or is stopping. */
  | "internal";

/** One error code. */
export interface ErrorInfo {
  code: ErrorCode;
  summary: string;
}

export interface ErrorMessage {
  id?: number;
  code: ErrorCode;
  message: string;
}

/** `EVENT`: an event and its number, from 1 per incarnation. */
export type Event = { seq: number } & (
  /** A session was created. */
  | { event: "session_created"; session: Session }
  /**
   * Label, `cols`/`rows`, `size` (policy or owner), title, pwd, `attachments`,
   * `attention` or `command` changed; not `last_input` alone.
   */
  | { event: "session_changed"; session: Session }
  /** The stream ended; `exit` is set. */
  | { event: "session_exited"; session: Session }
  /** Removed by request, or by `on_exit: remove`. */
  | { event: "session_removed"; id: string }
  /** One record was set; see Program status. */
  | { event: "program_status_changed"; id: string; record: StatusRecord; reason: StatusReason }
  /** Records were dropped or cleared. */
  | { event: "program_status_removed"; id: string; record_ids: string[]; reason: StatusReason }
  /** BEL, at most one per session per 100 ms. */
  | { event: "bell"; id: string }
  /** OSC 9 or OSC 777 `notify`; `title` empty when the sequence has none. */
  | { event: "notification"; id: string; title: string; body: string }
  /** OSC 9;4. */
  | { event: "progress"; id: string; state: ProgressState; progress?: number }
  /** OSC 133 `C`: the command's output starts. */
  | { event: "command_started"; id: string; offset: number }
  /** OSC 133 `D`, with the exit code the shell sent. */
  | { event: "command_ended"; id: string; offset: number; exit?: number }
  /** An `OPEN` attached. */
  | { event: "attachment_opened"; id: string; channel: number; client: Peer; read_only: boolean }
  /** The attachment ended. */
  | { event: "attachment_closed"; id: string; channel: number; reason: string }
);

/** One kind of `EVENT`. */
export interface EventInfo {
  /** The `event` field. */
  name: string;
  summary: string;
  /** JSON Schema of the event, without `seq`. */
  schema: unknown;
  /** `EVENT` payloads, with `seq`. */
  examples: unknown[];
}

/** A request's `params` and the `result` of its reply. */
export interface Example {
  params: unknown;
  result: unknown;
}

export type Exit = { code: number } | { signal: string };

export interface Exited {
  seq: number;
  offset: number;
  exit: Exit;
}

/** One frame type. */
export interface FrameInfo {
  name: string;
  /** The header's `type` byte. */
  type: number;
  direction: Direction;
  /** `0` for control, `n` for an attachment, `0 or n` for either. */
  channel: string;
  payload: Payload;
  summary: string;
  /** JSON Schema of a JSON payload. */
  schema?: unknown;
}

export interface Gap {
  from: number;
  to: number;
}

export interface Hello {
  /** Always 1. */
  version: number;
  /** Informational. */
  client: ClientPeer;
  /** Engine id; absent for control-only clients. */
  engine?: string;
  /** Capability ids; version 1 defines none. */
  capabilities?: string[];
}

export interface LabelParams {
  /** The session's id. */
  session: string;
  /** At most `limits.label` bytes. */
  label: string;
}

/** Server limits announced in `WELCOME`. Byte counts and milliseconds. */
export interface Limits {
  /** Payload bytes per frame. */
  max_frame: number;
  /** Client JSON payload bytes. */
  control_body: number;
  /** Retained stream bytes per session. */
  ring: number;
  /** Pending input bytes per session. */
  input_queue: number;
  /** Scrollback bytes per session. */
  scrollback: number;
  /** Continuation tracking bytes per session. */
  continuation: number;
  /** Label bytes. */
  label: number;
  /** Bytes per `cwd`, `title`, `pwd`, and `argv`. */
  text: number;
  /** Retained markers per session. */
  markers: number;
  /** Program status records per session. */
  records: number;
  /** Commands kept per session. */
  commands: number;
  /** Sessions per `list` or `subscribe` reply. */
  page: number;
  /** Milliseconds allowed for `HELLO`. */
  hello_timeout: number;
  /** Milliseconds before `on_exit: remove` removes. */
  remove_grace: number;
  /** Events retained for `subscribe {after}`. */
  events: number;
  /** Open attachments per session. */
  attachments: number;
}

export interface ListParams {
  /** The `next` of the previous page. */
  after?: string;
}

/** `wait.match`: the text that matched and the offset of its first byte. */
export interface Match {
  text: string;
  offset: number;
}

/** One method of `REQUEST`. */
export interface MethodInfo {
  name: string;
  class: Class;
  summary: string;
  /** JSON Schema of `params`; objects refuse undefined fields. */
  params: unknown;
  /** JSON Schema of `REPLY.result`; objects may gain fields. */
  result: unknown;
  /** The codes its `ERROR` can carry. */
  errors: ErrorCode[];
  examples: Example[];
}

export type OnExit = "remove" | "keep";

export interface Open {
  /** The session's id. */
  session: string;
  /** Default false. */
  read_only?: boolean;
  viewport?: Size;
  /** Colours as `#rrggbb`; `scheme` is `light` or `dark`. */
  profile?: Profile;
  resume?: Resume;
}

/** `until.output`: exactly one of `text` and `regex`. */
export interface OutputMatch {
  text?: string;
  /** Rust `regex` syntax, on bytes. */
  regex?: string;
}

export interface Page {
  /** Ordered by `id`, at most `limits.page`. */
  sessions: Session[];
  /** The last id, when more remain: the next `list`'s `after`. */
  next?: string;
}

/** What a frame's payload holds. */
export type Payload = "json" | "raw" | "empty";

/** `WELCOME.server`. */
export interface Peer {
  name: string;
  version: string;
}

export interface Profile {
  fg?: string;
  bg?: string;
  cursor?: string;
  scheme?: Scheme;
}

export type ProgressState = "remove" | "set" | "error" | "indeterminate" | "pause";

export interface Read {
  text: string;
  cols: number;
  rows: number;
  /** Zero-based; absent for a session loaded from a park file. */
  cursor?: Cursor;
  /** Stream offset at the time of the read. */
  offset: number;
  status: Status;
  /**
   * Present and true when lines were left out: cut to fit `max_frame`, or, for
   * `last_command`, bytes the stream no longer holds.
   */
  truncated?: boolean;
}

export type ReadFormat = "text" | "vt" | "html";

export interface ReadParams {
  /** The session's id. */
  session: string;
  /** History lines before the screen, for scope `screen`; default 0. */
  scrollback?: number;
  /** Default `text`. */
  format?: ReadFormat;
  /** Default `screen`. */
  scope?: ReadScope;
}

export type ReadScope = "screen" | "history" | "alternate" | "last_command";

export interface Reply {
  id: number;
  result: unknown;
}

export interface Request {
  id: number;
  method: string;
  params: unknown;
}

/**
 * `resize`: a side left out keeps the session's; `policy` left out keeps the
 * session's.
 */
export interface ResizeParams {
  /** The session's id. */
  session: string;
  cols?: number;
  rows?: number;
  policy?: SizePolicy;
}

export interface Resized {
  seq: number;
  offset: number;
  cols: number;
  rows: number;
}

export interface Resume {
  incarnation: string;
  offset: number;
  marker: number;
}

export type Scheme = "light" | "dark";

/** `send`: exactly one of `text`, `keys` and `data`. */
export interface SendParams {
  /** The session's id. */
  session: string;
  /** UTF-8, written as typed. */
  text?: string;
  /** Key names, encoded by the session's terminal. */
  keys?: string[];
  /** Base64 bytes. */
  data?: string;
  /**
   * `text` or `data` as a paste, bracketed when mode 2004 is set; default
   * false.
   */
  paste?: boolean;
  /** A wait armed before the bytes are written. */
  wait?: SendWait;
}

/**
 * `send.wait`: a `wait` on the same session, from the cursor before the bytes.
 */
export interface SendWait {
  until: Until;
  /** None: no limit. */
  timeout_ms?: number;
}

/** The `send` result. */
export interface Sent {
  /** The session's cursor when the bytes were queued. */
  at: StreamCursor;
  /** With `send.wait`: its result. */
  wait?: Waited;
}

export interface Session {
  /** UUID assigned by the server, never reused. */
  id: string;
  /** At most `limits.label` bytes. */
  label: string;
  status: Status;
  argv: string[];
  /** Working directory at creation. */
  cwd: string;
  /** While the child is alive. */
  pid?: number;
  /** Current size. */
  cols: number;
  /** Current size. */
  rows: number;
  /** From the program, OSC 0 or 2. */
  title: string;
  /** From the program, OSC 7; the path only, host dropped, percent-decoded. */
  pwd: string;
  on_exit: OnExit;
  /** RFC 3339. */
  created_at: string;
  /** RFC 3339. */
  exited_at?: string;
  /** `signal` by name. */
  exit?: Exit;
  /** Current stream offset. */
  offset: number;
  /** Sequence number of the latest marker, 0 if none. */
  marker: number;
  /** Open attachments, in the order they opened. */
  attachments: AttachmentInfo[];
  /** The command running or the last that ran; see Commands. */
  command?: Command;
  /** Only in `inspect`; see Program status. */
  program_status?: StatusRecord[];
  /** The record that most wants the user; see Program status. */
  attention?: Attention;
  /**
   * The size policy, and under `latest` the `channel` of the attachment whose
   * report is current; see Size.
   */
  size: SizeInfo;
}

export interface SessionParams {
  /** The session's id. */
  session: string;
}

export interface SignalParams {
  /** The session's id. */
  session: string;
  /** A name: `HUP`, `INT`, `TERM`, `KILL`, ... */
  signal: string;
}

/** `OPEN.viewport`: the terminal size in cells and, optionally, pixels. */
export interface Size {
  cols: number;
  rows: number;
  cell_width?: number;
  cell_height?: number;
}

/**
 * `Session.size`: the policy and, under `latest`, the attachment whose report
 * is current.
 */
export interface SizeInfo {
  policy: SizePolicy;
  owner?: number;
}

/** How a session's size follows its attachments' viewport reports. */
export type SizePolicy =
  /** The most recent report wins. */
  | "latest"
  /** The smallest columns and rows among the attachments' reports. */
  | "smallest"
  /** Only `resize` changes the size. */
  | "fixed";

export type Status = "running" | "exited" | "restored";

/** `until.status`: a record in one of `states`, or the record `record`. */
export interface StatusMatch {
  /** `idle`, `working`, `done`, `blocked`, `error`. */
  states: string[];
  /** Only the record with this id. */
  record?: string;
}

/** Why a program status record was set or removed. */
export type StatusReason = "report" | "clear" | "prompt" | "exit" | "restore" | "evicted" | "ack";

/** One program status record (OSC 7501). */
export interface StatusRecord {
  /** The record id; `""` for the root. */
  id: string;
  /** `idle`, `working`, `done`, `blocked` or `error`. */
  state: string;
  kind?: string;
  /**
   * In `inspect`, resolved from the nearest ancestor when the record has none.
   */
  app?: string;
  title?: string;
  msg?: string;
  /** 0 to 100. */
  progress?: number;
  /** RFC 3339. */
  updated_at: string;
  /** On `done` and `error` records only: false until `ack`. */
  seen?: boolean;
}

/**
 * A stream cursor as the server reports it: the next byte's offset and the
 * latest marker reflected.
 */
export interface StreamCursor {
  offset: number;
  marker: number;
}

/**
 * `subscribe`: `after` and `incarnation` together replay the events after
 * `after`; neither is the first page.
 */
export interface SubscribeParams {
  /** The `seq` of the last event the client applied. */
  after?: number;
  /** The `WELCOME.incarnation` `after` was counted in. */
  incarnation?: string;
}

/** The `subscribe` reply: the first page without `after`, `gap` with it. */
export interface Subscribed {
  /** Without `after`: the first page of sessions. */
  sessions?: Session[];
  /** Without `after`: as in Page. */
  next?: string;
  /** The `seq` of the oldest event retained, `latest + 1` when none is. */
  oldest: number;
  /** The `seq` of the latest event, 0 before the first. */
  latest: number;
  /**
   * With `after`: the events after it are not all retained, or `incarnation` is
   * not the server's; none is replayed.
   */
  gap?: boolean;
}

/** `tail`'s result. */
export interface Tail {
  /**
   * Plain text: the stream without escape sequences, and without controls but
   * LF, CR and TAB; see `tail`.
   */
  text: string;
  /** Where the text starts: `after`, or later when truncated. */
  start_offset: number;
  /** Where it ends; the next `after`. */
  next_offset: number;
  /**
   * Nothing more to read now: `next_offset` is the current stream offset, or
   * what follows it is unfinished.
   */
  complete: boolean;
  /** Present and true when `start_offset` is above `after`. */
  truncated?: boolean;
}

export interface TailParams {
  /** The session's id. */
  session: string;
  /** The stream offset to read from. */
  after: number;
  /** Stream bytes to cover, at least 1; default 65536. */
  max_bytes?: number;
}

/** What a wait waits for: one or more facts, the first to hold wins. */
export interface Until {
  /** `true`: the stream ends. */
  exited?: boolean;
  output?: OutputMatch;
  status?: StatusMatch;
  /** `true`: the next OSC 133 `A`. */
  prompt?: boolean;
  /** No output for this long. */
  quiet_ms?: number;
}

/** The `VIEWPORT` payload: a size and a profile in one flat object. */
export interface Viewport {
  cols: number;
  rows: number;
  cell_width?: number;
  cell_height?: number;
  profile?: Profile;
}

export interface WaitParams {
  /** The session's id. */
  session: string;
  until: Until;
  /** Default: the session's cursor when the request is handled. */
  after?: CursorParams;
  /** None: no limit. */
  timeout_ms?: number;
}

export type WaitReason = "exited" | "matched" | "status" | "prompt" | "quiet" | "timeout";

/** The `wait` result. */
export interface Waited {
  reason: WaitReason;
  /**
   * Just past the match; after the output holding the prompt; at the end of the
   * stream; else the cursor when it ended. Past a match or a prompt, `marker`
   * is 0 when a later marker is no longer retained.
   */
  at: StreamCursor;
  /** `exited`: the session's `exit`. */
  exit?: Exit;
  /**
   * `matched`: the plain text that matched and the stream offset of its first
   * byte.
   */
  match?: Match;
  /** `status`: the record, as `inspect` shows it. */
  record?: StatusRecord;
}

export interface Welcome {
  /** Always 1. */
  version: number;
  server: Peer;
  /** Engine id of the server. */
  engine: string;
  /** UUID, new on every server start. */
  incarnation: string;
  /** Unknown ids are ignored. */
  capabilities: string[];
  /** The server's limits. */
  limits: Limits;
}
