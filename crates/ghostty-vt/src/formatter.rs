//! Format a terminal's active screen as plain text or VT sequences.

use std::ptr;

use ghostty_vt_sys as sys;

use crate::error::{Result, check};
use crate::io;
use crate::terminal::Term;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Plain,
    Vt,
}

/// Formatting options. `Default` is plain text of the whole screen with
/// nothing extra. The extras apply to VT output only.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub format: Format,
    /// Join soft-wrapped rows into one line.
    pub unwrap: bool,
    /// Drop trailing blanks.
    pub trim: bool,
    pub cursor: bool,
    pub style: bool,
    pub hyperlink: bool,
    pub palette: bool,
    pub modes: bool,
    pub scrolling_region: bool,
    pub keyboard: bool,
}

pub fn format(term: &Term, options: Options) -> Result<Vec<u8>> {
    let raw_options = sys::GhosttyFormatterTerminalOptions {
        size: size_of::<sys::GhosttyFormatterTerminalOptions>(),
        emit: match options.format {
            Format::Plain => sys::GHOSTTY_FORMATTER_FORMAT_PLAIN,
            Format::Vt => sys::GHOSTTY_FORMATTER_FORMAT_VT,
        },
        unwrap: options.unwrap,
        trim: options.trim,
        extra: sys::GhosttyFormatterTerminalExtra {
            size: size_of::<sys::GhosttyFormatterTerminalExtra>(),
            palette: options.palette,
            modes: options.modes,
            scrolling_region: options.scrolling_region,
            tabstops: false,
            pwd: false,
            keyboard: options.keyboard,
            screen: sys::GhosttyFormatterScreenExtra {
                size: size_of::<sys::GhosttyFormatterScreenExtra>(),
                cursor: options.cursor,
                style: options.style,
                hyperlink: options.hyperlink,
                protection: false,
                kitty_keyboard: options.keyboard,
                charsets: false,
            },
        },
        selection: ptr::null(),
    };
    let mut formatter = ptr::null_mut();
    // SAFETY: term outlives the formatter, which is freed before returning.
    check(unsafe {
        sys::ghostty_formatter_terminal_new(ptr::null(), &mut formatter, term.raw(), raw_options)
    })?;
    let mut out = Vec::new();
    // SAFETY: the writer borrows `out` only for the duration of this call.
    let result = unsafe { sys::ghostty_formatter_format(formatter, io::writer(&mut out)) };
    // SAFETY: formatter was created above and is freed once.
    unsafe { sys::ghostty_formatter_free(formatter) };
    check(result)?;
    Ok(out)
}
