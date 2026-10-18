//! Safe wrapper over the vendored libghostty-vt.

mod error;
pub mod formatter;
mod io;
pub mod snapshot;
mod terminal;

pub use error::{Error, Result};
pub use terminal::*;

/// Engine id of the vendored build, `ghostty-vt@<vendored VERSION>`.
pub const ENGINE: &str = concat!("ghostty-vt@", env!("GHOSTTY_VT_VERSION"));

#[cfg(test)]
mod tests;
