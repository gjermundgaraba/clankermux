#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]
#![allow(clippy::all, clippy::pedantic)]

include!("bindings.rs");

/// The engine id of this build: `ghostty-vt@` and the vendored version.
pub const ENGINE: &str = concat!("ghostty-vt@", env!("GHOSTTY_VT_VERSION"));
