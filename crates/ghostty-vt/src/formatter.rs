//! Format a terminal's active screen as plain text, VT sequences or HTML.

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
    /// HTML with inline styles.
    Html,
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
    /// The palette: in VT, OSC 4 sequences; in HTML, the
    /// `--vt-palette-N` CSS variables its colours refer to.
    pub palette: bool,
    pub modes: bool,
    pub scrolling_region: bool,
    pub keyboard: bool,
    pub region: Region,
}

/// The part of the terminal to format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Region {
    /// History and screen.
    #[default]
    All,
    /// From this row (counted from the top of the scrollback) to the end
    /// of the screen.
    FromRow(u32),
    /// A `cols` by `rows` window of the screen starting at its row
    /// `from_row`, left-aligned.
    Clip { from_row: u16, cols: u16, rows: u16 },
}

pub fn format(term: &Term, options: Options) -> Result<Vec<u8>> {
    let mut raw_options = sys::GhosttyFormatterTerminalOptions {
        size: size_of::<sys::GhosttyFormatterTerminalOptions>(),
        emit: match options.format {
            Format::Plain => sys::GHOSTTY_FORMATTER_FORMAT_PLAIN,
            Format::Vt => sys::GHOSTTY_FORMATTER_FORMAT_VT,
            Format::Html => sys::GHOSTTY_FORMATTER_FORMAT_HTML,
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
    let active = sys::GHOSTTY_POINT_TAG_ACTIVE;
    let selection = match options.region {
        Region::All => None,
        Region::FromRow(row) => Some(selection(
            point(term, sys::GHOSTTY_POINT_TAG_SCREEN, 0, row)?,
            point(
                term,
                active,
                term.cols().saturating_sub(1),
                u32::from(term.rows().saturating_sub(1)),
            )?,
            false,
        )),
        Region::Clip {
            from_row,
            cols,
            rows,
        } => {
            let top = from_row.min(term.rows().saturating_sub(1));
            let bottom = (u32::from(top) + u32::from(rows)).min(u32::from(term.rows())) - 1;
            Some(selection(
                point(term, active, 0, u32::from(top))?,
                point(
                    term,
                    active,
                    cols.min(term.cols()).saturating_sub(1),
                    bottom,
                )?,
                true,
            ))
        }
    };
    if let Some(selection) = &selection {
        raw_options.selection = selection;
    }
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

/// The grid reference at a point. Valid until the terminal changes.
fn point(term: &Term, tag: sys::GhosttyPointTag, x: u16, y: u32) -> Result<sys::GhosttyGridRef> {
    let point = sys::GhosttyPoint {
        tag,
        value: sys::GhosttyPointValue {
            coordinate: sys::GhosttyPointCoordinate { x, y },
        },
    };
    let mut out = sys::GhosttyGridRef {
        size: size_of::<sys::GhosttyGridRef>(),
        ..Default::default()
    };
    // SAFETY: term is live and out is a sized GhosttyGridRef.
    check(unsafe { sys::ghostty_terminal_grid_ref(term.raw(), point, &mut out) })?;
    Ok(out)
}

fn selection(
    start: sys::GhosttyGridRef,
    end: sys::GhosttyGridRef,
    rectangle: bool,
) -> sys::GhosttySelection {
    sys::GhosttySelection {
        size: size_of::<sys::GhosttySelection>(),
        start,
        end,
        rectangle,
    }
}
