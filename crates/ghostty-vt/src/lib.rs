mod error;
pub mod formatter;
mod io;
pub mod keys;
pub mod snapshot;
mod terminal;

pub use error::{Error, Result};
pub use ghostty_vt_sys::ENGINE;
pub use terminal::*;

#[cfg(test)]
mod tests;
