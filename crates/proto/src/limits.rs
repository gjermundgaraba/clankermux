use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;

/// Server limits announced in `WELCOME`. Byte counts and milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_frame: u64,
    pub control_body: u64,
    pub ring: u64,
    pub input_queue: u64,
    pub scrollback: u64,
    pub continuation: u64,
    pub label: u64,
    pub text: u64,
    pub markers: u64,
    pub records: u64,
    pub page: u64,
    pub hello_timeout: u64,
    pub remove_grace: u64,
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
            records: 200,
            page: 32,
            hello_timeout: 10_000,
            remove_grace: 10_000,
        }
    }
}
