use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;

/// Server limits announced in `WELCOME`. Byte counts and milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Limits {
    /// Payload bytes per frame.
    pub max_frame: u64,
    /// Client JSON payload bytes.
    pub control_body: u64,
    /// Retained stream bytes per session.
    pub ring: u64,
    /// Pending input bytes per session.
    pub input_queue: u64,
    /// Scrollback bytes per session.
    pub scrollback: u64,
    /// Continuation tracking bytes per session.
    pub continuation: u64,
    /// Label bytes.
    pub label: u64,
    /// Bytes per `cwd`, `title`, `pwd`, and `argv`.
    pub text: u64,
    /// Retained markers per session.
    pub markers: u64,
    /// Program status records per session.
    pub records: u64,
    /// Commands kept per session.
    pub commands: u64,
    /// Sessions per `list` or `subscribe` reply.
    pub page: u64,
    /// Milliseconds allowed for `HELLO`.
    pub hello_timeout: u64,
    /// Milliseconds before `on_exit: remove` removes.
    pub remove_grace: u64,
    /// Events retained for `subscribe {after}`.
    pub events: u64,
    /// Open attachments per session.
    pub attachments: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_frame: MIB,
            control_body: 64 * 1024,
            ring: 4 * MIB,
            input_queue: MIB,
            scrollback: 10 * MIB,
            continuation: 8 * MIB,
            label: 256,
            text: 2048,
            markers: 1024,
            records: 198,
            commands: 64,
            page: 32,
            hello_timeout: 10_000,
            remove_grace: 10_000,
            events: 4096,
            attachments: 16,
        }
    }
}
