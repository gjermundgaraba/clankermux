pub mod catalog;
mod frame;
mod limits;
mod message;
pub mod render;
pub mod typescript;

pub use frame::*;
pub use limits::{Limits, cap};
pub use message::*;

pub const VERSION: u32 = 1;

#[cfg(test)]
mod props;
#[cfg(test)]
mod tests;
