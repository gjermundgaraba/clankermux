//! JSON payloads. What a client sends refuses unknown fields; what the
//! server sends may grow, and unknown fields are ignored on receipt.
//! Optional fields are omitted when absent.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Limits;

/// `WELCOME.server`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Peer {
    pub name: String,
    pub version: String,
}

/// `HELLO.client`: a `Peer` as the client sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientPeer {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// Always 1.
    pub version: u32,
    /// Informational.
    pub client: ClientPeer,
    /// Engine id; absent for control-only clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Capability ids; version 1 defines none.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Welcome {
    /// Always 1.
    pub version: u32,
    pub server: Peer,
    /// Engine id of the server.
    pub engine: String,
    /// UUID, new on every server start.
    pub incarnation: String,
    /// Unknown ids are ignored.
    pub capabilities: Vec<String>,
    /// The server's limits.
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: i64,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Reply {
    pub id: i64,
    pub result: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A connection error under Framing; the connection closes.
    ProtocolError,
    /// `HELLO.version` is not 1; the connection closes.
    VersionMismatch,
    /// Malformed payload, a field the protocol does not define, unknown
    /// method, a resume marker above the session's latest, a `subscribe`
    /// `after` above the latest event, `read` with `scrollback` and a
    /// scope other than `screen`, or `tail` from past the stream's end or
    /// with `max_bytes` 0.
    InvalidRequest,
    /// `label`, `text` or `attachments` exceeded, or a snapshot failed.
    LimitExceeded,
    /// No session has the id.
    NoSuchSession,
    /// `detach` named no open attachment of the session.
    NoSuchAttachment,
    /// `status` is not `running`, or the stream has ended; for a wait,
    /// nothing matched before the stream ended.
    NotRunning,
    /// `remove` on a running session.
    Running,
    /// `end` or `signal` found no process to signal.
    SignalFailed,
    /// Engine ids differ.
    EngineMismatch,
    /// `OPEN` on a channel already in use.
    ChannelInUse,
    /// `INPUT` or `VIEWPORT` on a read-only attachment.
    ReadOnly,
    /// The input would exceed `limits.input_queue`.
    InputOverflow,
    /// Second gap during a bootstrap.
    TooSlow,
    /// A `cancel` ended the request.
    Cancelled,
    /// `read` of `alternate` off the alternate screen, or of
    /// `last_command` with no command output marked.
    ScopeUnavailable,
    /// The server failed, or is stopping.
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnExit {
    Remove,
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Exited,
    Restored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Exit {
    Code { code: i32 },
    Signal { signal: String },
}

/// One program status record (OSC 7501).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusRecord {
    /// The record id; `""` for the root.
    pub id: String,
    /// `idle`, `working`, `done`, `blocked` or `error`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// In `inspect`, resolved from the nearest ancestor when the record
    /// has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    /// 0 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    /// RFC 3339.
    pub updated_at: String,
    /// On `done` and `error` records only: false until `ack`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<bool>,
}

/// `Session.attention`: the most urgent record that wants the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Attention {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    /// The record's id.
    pub record: String,
}

/// One command from its OSC 133 marks, as stream offsets: `start` at
/// the prompt (`A`), `output` where its output begins (`C`), `end` where
/// it ended (`D`) with the `exit` the shell reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Command {
    pub start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    /// UUID assigned by the server, never reused.
    pub id: String,
    /// At most `limits.label` bytes.
    pub label: String,
    pub status: Status,
    pub argv: Vec<String>,
    /// Working directory at creation.
    pub cwd: String,
    /// While the child is alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Current size.
    pub cols: u16,
    /// Current size.
    pub rows: u16,
    /// From the program, OSC 0 or 2.
    pub title: String,
    /// From the program, OSC 7; the path only, host dropped,
    /// percent-decoded.
    pub pwd: String,
    pub on_exit: OnExit,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<String>,
    /// `signal` by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
    /// Current stream offset.
    pub offset: u64,
    /// Sequence number of the latest marker, 0 if none.
    pub marker: u64,
    /// Open attachments, in the order they opened.
    pub attachments: Vec<AttachmentInfo>,
    /// The command running or the last that ran; see Commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Command>,
    /// Only in `inspect`; see Program status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_status: Option<Vec<StatusRecord>>,
    /// The record that most wants the user; see Program status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<Attention>,
    /// The size policy, and under `latest` the `channel` of the
    /// attachment whose report is current; see Size.
    #[serde(default)]
    pub size: SizeInfo,
}

/// One open attachment of a session, as `list` and `inspect` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentInfo {
    /// Assigned by the server at `OPEN`, unique on it and never reused
    /// while it runs; not the frame channel, which only its connection
    /// knows. `detach` takes it.
    pub channel: u64,
    /// From the connection's `HELLO.client`, each cut to 32 bytes with
    /// control characters replaced by `?`.
    pub client: Peer,
    pub read_only: bool,
    /// Its latest viewport report, in `OPEN` or `VIEWPORT`; absent until
    /// it makes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Cells>,
    /// When it opened, RFC 3339.
    pub since: String,
    /// When it last sent `INPUT` the server accepted, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input: Option<String>,
}

/// A size in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Cells {
    pub cols: u16,
    pub rows: u16,
}

/// How a session's size follows its attachments' viewport reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SizePolicy {
    /// The most recent report wins.
    #[default]
    Latest,
    /// The smallest columns and rows among the attachments' reports.
    Smallest,
    /// Only `resize` changes the size.
    Fixed,
}

/// `Session.size`: the policy and, under `latest`, the attachment whose
/// report is current.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SizeInfo {
    pub policy: SizePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<u64>,
}

/// The result of a method that answers with success alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Empty {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Page {
    /// Ordered by `id`, at most `limits.page`.
    pub sessions: Vec<Session>,
    /// The last id, when more remain: the next `list`'s `after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Read {
    pub text: String,
    pub cols: u16,
    pub rows: u16,
    /// Zero-based; absent for a session loaded from a park file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// Stream offset at the time of the read.
    pub offset: u64,
    pub status: Status,
    /// Present and true when lines were left out: cut to fit
    /// `max_frame`, or, for `last_command`, bytes the stream no longer
    /// holds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// `tail`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Tail {
    /// Plain text: the stream without escape sequences, and without
    /// controls but LF, CR and TAB; see `tail`.
    pub text: String,
    /// Where the text starts: `after`, or later when truncated.
    pub start_offset: u64,
    /// Where it ends; the next `after`.
    pub next_offset: u64,
    /// Nothing more to read now: `next_offset` is the current stream
    /// offset, or what follows it is unfinished.
    pub complete: bool,
    /// Present and true when `start_offset` is above `after`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateParams {
    /// At most `limits.label` bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Default: the user's login shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    /// Default: the server user's home directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Additions and overrides of the server's environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// Default 80.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub cols: Option<u16>,
    /// Default 24.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub rows: Option<u16>,
    /// Default `remove`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_exit: Option<OnExit>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    /// The `next` of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// `subscribe`: `after` and `incarnation` together replay the events
/// after `after`; neither is the first page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubscribeParams {
    /// The `seq` of the last event the client applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// The `WELCOME.incarnation` `after` was counted in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<String>,
}

/// The `subscribe` reply: the first page without `after`, `gap` with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Subscribed {
    /// Without `after`: the first page of sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<Session>>,
    /// Without `after`: as in Page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// The `seq` of the oldest event retained, `latest + 1` when none is.
    pub oldest: u64,
    /// The `seq` of the latest event, 0 before the first.
    pub latest: u64,
    /// With `after`: the events after it are not all retained, or
    /// `incarnation` is not the server's; none is replayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionParams {
    /// The session's id.
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalParams {
    /// The session's id.
    pub session: String,
    /// A name: `HUP`, `INT`, `TERM`, `KILL`, ...
    pub signal: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelParams {
    /// The session's id.
    pub session: String,
    /// At most `limits.label` bytes.
    pub label: String,
}

/// `resize`: a side left out keeps the session's; `policy` left out
/// keeps the session's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResizeParams {
    /// The session's id.
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<SizePolicy>,
}

/// `detach`: `reason` goes to the attachment as `CLOSED.message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DetachParams {
    /// The session's id.
    pub session: String,
    /// The attachment's `channel`, as `inspect` lists it.
    pub channel: u64,
    /// Told to the attachment; at most `limits.text` bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    /// The session's id.
    pub session: String,
    /// History lines before the screen, for scope `screen`; default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<u32>,
    /// Default `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ReadFormat>,
    /// Default `screen`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ReadScope>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadFormat {
    #[default]
    Text,
    Vt,
    Html,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadScope {
    #[default]
    Screen,
    History,
    Alternate,
    LastCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TailParams {
    /// The session's id.
    pub session: String,
    /// The stream offset to read from.
    pub after: u64,
    /// Stream bytes to cover, at least 1; default 65536.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_bytes: Option<u64>,
}

/// `send`: exactly one of `text`, `keys` and `data`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendParams {
    /// The session's id.
    pub session: String,
    /// UTF-8, written as typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Key names, encoded by the session's terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
    /// Base64 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// `text` or `data` as a paste, bracketed when mode 2004 is set;
    /// default false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paste: bool,
    /// A wait armed before the bytes are written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<SendWait>,
}

/// `send.wait`: a `wait` on the same session, from the cursor before the
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendWait {
    pub until: Until,
    /// None: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// What a wait waits for: one or more facts, the first to hold wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Until {
    /// `true`: the stream ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusMatch>,
    /// `true`: the next OSC 133 `A`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<bool>,
    /// No output for this long.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_ms: Option<u64>,
}

/// `until.output`: exactly one of `text` and `regex`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub text: Option<String>,
    /// Rust `regex` syntax, on bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

/// `until.status`: a record in one of `states`, or the record `record`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusMatch {
    /// `idle`, `working`, `done`, `blocked`, `error`.
    #[schemars(length(min = 1))]
    pub states: Vec<String>,
    /// Only the record with this id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
}

/// A stream cursor as a request gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CursorParams {
    pub offset: u64,
    pub marker: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitParams {
    /// The session's id.
    pub session: String,
    pub until: Until,
    /// Default: the session's cursor when the request is handled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<CursorParams>,
    /// None: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelParams {
    /// The pending request's id on this connection.
    pub id: i64,
}

/// A stream cursor as the server reports it: the next byte's offset and
/// the latest marker reflected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StreamCursor {
    pub offset: u64,
    pub marker: u64,
}

/// The `send` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Sent {
    /// The session's cursor when the bytes were queued.
    pub at: StreamCursor,
    /// With `send.wait`: its result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Waited>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    Exited,
    Matched,
    Status,
    Prompt,
    Quiet,
    Timeout,
}

/// The `wait` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Waited {
    pub reason: WaitReason,
    /// Just past the match; after the output holding the prompt; at the
    /// end of the stream; else the cursor when it ended.
    pub at: StreamCursor,
    /// `exited`: the session's `exit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
    /// `matched`: the plain text that matched and the stream offset of
    /// its first byte.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<Match>,
    /// `status`: the record, as `inspect` shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<StatusRecord>,
}

/// `wait.match`: the text that matched and the offset of its first byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Match {
    pub text: String,
    pub offset: u64,
}

/// The `cancel` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Cancelled {
    pub cancelled: bool,
}

/// `ack`: every `done` and `error` record when `record_ids` is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AckParams {
    /// The session's id.
    pub session: String,
    /// Default: every `done` and `error` record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_ids: Option<Vec<String>>,
}

/// The `ack` reply: the records it marked seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Acked {
    pub record_ids: Vec<String>,
}

/// Why a program status record was set or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusReason {
    Report,
    Clear,
    Prompt,
    Exit,
    Restore,
    Evicted,
    Ack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A session was created.
    SessionCreated { session: Session },
    /// Label, `cols`/`rows`, `size` (policy or owner), title, pwd,
    /// `attachments`, `attention` or `command` changed; not `last_input`
    /// alone.
    SessionChanged { session: Session },
    /// The stream ended; `exit` is set.
    SessionExited { session: Session },
    /// Removed by request, or by `on_exit: remove`.
    SessionRemoved { id: String },
    /// One record was set; see Program status.
    ProgramStatusChanged {
        id: String,
        record: StatusRecord,
        reason: StatusReason,
    },
    /// Records were dropped or cleared.
    ProgramStatusRemoved {
        id: String,
        record_ids: Vec<String>,
        reason: StatusReason,
    },
    /// BEL, at most one per session per 100 ms.
    Bell { id: String },
    /// OSC 9 or OSC 777 `notify`; `title` empty when the sequence has
    /// none.
    Notification {
        id: String,
        title: String,
        body: String,
    },
    /// OSC 9;4.
    Progress {
        id: String,
        state: ProgressState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<u8>,
    },
    /// OSC 133 `C`: the command's output starts.
    CommandStarted { id: String, offset: u64 },
    /// OSC 133 `D`, with the exit code the shell sent.
    CommandEnded {
        id: String,
        offset: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<i32>,
    },
    /// An `OPEN` attached.
    AttachmentOpened {
        id: String,
        channel: u64,
        client: Peer,
        read_only: bool,
    },
    /// The attachment ended.
    AttachmentClosed {
        id: String,
        channel: u64,
        reason: String,
    },
}

/// `EVENT`: an event and its number, from 1 per incarnation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
// The schema has the payload's name; `Event` is only flattened into it.
#[schemars(rename = "Event")]
pub struct Numbered {
    pub seq: u64,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    Remove,
    Set,
    Error,
    Indeterminate,
    Pause,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<Scheme>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    Light,
    Dark,
}

/// `OPEN.viewport`: the terminal size in cells and, optionally, pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_height: Option<u32>,
}

/// The `VIEWPORT` payload: a size and a profile in one flat object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "ViewportWire", into = "ViewportWire")]
pub struct Viewport {
    pub size: Size,
    pub profile: Option<Profile>,
}

/// `Viewport` on the wire: `flatten` would not refuse unknown fields.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ViewportWire {
    cols: u16,
    rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cell_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cell_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<Profile>,
}

impl From<ViewportWire> for Viewport {
    fn from(w: ViewportWire) -> Self {
        Viewport {
            size: Size {
                cols: w.cols,
                rows: w.rows,
                cell_width: w.cell_width,
                cell_height: w.cell_height,
            },
            profile: w.profile,
        }
    }
}

impl From<Viewport> for ViewportWire {
    fn from(v: Viewport) -> Self {
        ViewportWire {
            cols: v.size.cols,
            rows: v.size.rows,
            cell_width: v.size.cell_width,
            cell_height: v.size.cell_height,
            profile: v.profile,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resume {
    pub incarnation: String,
    pub offset: u64,
    pub marker: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Open {
    /// The session's id.
    pub session: String,
    /// Default false.
    #[serde(default)]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Size>,
    /// Colours as `#rrggbb`; `scheme` is `light` or `dark`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    Snapshot,
    Resume,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Attached {
    pub mode: AttachMode,
    /// Cursor offset the attachment continues from.
    pub offset: u64,
    /// Cursor marker: latest `seq` reflected in the snapshot, or in the
    /// client's terminal when resuming.
    pub marker: u64,
    pub cols: u16,
    pub rows: u16,
    pub session: Session,
    /// Required in mode `snapshot`, absent otherwise: the total length of
    /// the snapshot that follows (the sum of the `SNAPSHOT` payloads), so
    /// a client can size its buffer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Resized {
    pub seq: u64,
    pub offset: u64,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Exited {
    pub seq: u64,
    pub offset: u64,
    pub exit: Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Gap {
    pub from: u64,
    pub to: u64,
}

/// Displays a wire enum as its JSON name.
macro_rules! wire_display {
    ($($t:ty),*) => {$(
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let value = serde_json::to_value(self).expect("serializable");
                f.write_str(value.as_str().expect("a string"))
            }
        }
    )*};
}

pub(crate) use wire_display;

wire_display!(
    ErrorCode,
    Status,
    ClosedReason,
    WaitReason,
    ReadFormat,
    ReadScope,
    SizePolicy
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClosedReason {
    Closed,
    Removed,
    Stopping,
    Detached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Closed {
    pub reason: ClosedReason,
    /// The `reason` a `detach` gave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
