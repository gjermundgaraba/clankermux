//! Wire types of the clankermux protocol, version 1: framing, JSON
//! messages, limits and error codes. No I/O and no engine dependency.

pub mod frame;
mod limits;
mod message;

pub use frame::*;
pub use limits::Limits;
pub use message::*;

pub const VERSION: u32 = 1;

#[cfg(test)]
mod tests;
