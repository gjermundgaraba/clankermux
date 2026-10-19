//! JSON payloads. What a client sends refuses unknown fields; what the
//! server sends may grow, and unknown fields are ignored on receipt.
//! Optional fields are omitted when absent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Limits;

/// `WELCOME.server`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub version: String,
}

/// `HELLO.client`: a `Peer` as the client sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPeer {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub version: u32,
    pub client: ClientPeer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub version: u32,
    pub server: Peer,
    pub engine: String,
    pub incarnation: String,
    pub capabilities: Vec<String>,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: i64,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub id: i64,
    pub result: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolError,
    VersionMismatch,
    InvalidRequest,
    LimitExceeded,
    NoSuchSession,
    NotRunning,
    Running,
    SignalFailed,
    EngineMismatch,
    ChannelInUse,
    ReadOnly,
    InputOverflow,
    TooSlow,
    Cancelled,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExit {
    Remove,
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Exited,
    Restored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Exit {
    Code { code: i32 },
    Signal { signal: String },
}

/// One program status record (OSC 7501).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRecord {
    pub id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub label: String,
    pub status: Status,
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub pwd: String,
    pub on_exit: OnExit,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
    pub offset: u64,
    pub marker: u64,
    pub attachments: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_status: Option<Vec<StatusRecord>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub sessions: Vec<Session>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Read {
    pub text: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    pub offset: u64,
    pub status: Status,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_exit: Option<OnExit>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// `subscribe` takes no parameters: `{}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionParams {
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalParams {
    pub session: String,
    pub signal: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelParams {
    pub session: String,
    pub label: String,
}

/// `resize`: a side left out keeps the session's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResizeParams {
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<u32>,
}

/// `send`: exactly one of `text`, `keys` and `data`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendParams {
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
    /// `text` or `data` as a paste, bracketed when mode 2004 is set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paste: bool,
    /// A wait armed before the bytes are written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<SendWait>,
}

/// `send.wait`: a `wait` on the same session, from the cursor before the
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendWait {
    pub until: Until,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// What a wait waits for: one or more facts, the first to hold wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

/// `until.status`: a record in one of `states`, or the record `record`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusMatch {
    pub states: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
}

/// A stream cursor as a request gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorParams {
    pub offset: u64,
    pub marker: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitParams {
    pub session: String,
    pub until: Until,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<CursorParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelParams {
    pub id: i64,
}

/// A stream cursor as the server reports it: the next byte's offset and
/// the latest marker reflected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCursor {
    pub offset: u64,
    pub marker: u64,
}

/// The `send` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    /// The cursor once the PTY took the bytes.
    pub at: StreamCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Waited>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waited {
    pub reason: WaitReason,
    pub at: StreamCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<Match>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<StatusRecord>,
}

/// `wait.match`: the text that matched and the offset of its first byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    pub text: String,
    pub offset: u64,
}

/// The `cancel` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancelled {
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    SessionCreated { session: Session },
    SessionChanged { session: Session },
    SessionExited { session: Session },
    SessionRemoved { id: String },
    ProgramStatusChanged { id: String, record: StatusRecord },
    ProgramStatusRemoved { id: String, record_ids: Vec<String> },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    Light,
    Dark,
}

/// `OPEN.viewport`: the terminal size in cells and, optionally, pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ViewportWire", into = "ViewportWire")]
pub struct Viewport {
    pub size: Size,
    pub profile: Option<Profile>,
}

/// `Viewport` on the wire: `flatten` would not refuse unknown fields.
#[derive(Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resume {
    pub incarnation: String,
    pub offset: u64,
    pub marker: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub session: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Size>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    Snapshot,
    Resume,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attached {
    pub mode: AttachMode,
    pub offset: u64,
    pub marker: u64,
    pub cols: u16,
    pub rows: u16,
    pub session: Session,
    /// Total length of the snapshot that follows, in mode `snapshot`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resized {
    pub seq: u64,
    pub offset: u64,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exited {
    pub seq: u64,
    pub offset: u64,
    pub exit: Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

wire_display!(ErrorCode, Status, ClosedReason, WaitReason);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosedReason {
    Closed,
    Removed,
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    pub reason: ClosedReason,
}
