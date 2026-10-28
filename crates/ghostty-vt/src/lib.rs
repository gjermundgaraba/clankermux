mod error;
pub mod formatter;
mod io;
pub mod keys;
pub mod snapshot;
mod terminal;

pub use error::{Error, Result};
pub use terminal::*;

pub const ENGINE: &str = concat!("ghostty-vt@", env!("GHOSTTY_VT_VERSION"));

#[cfg(test)]
mod tests;
