use std::ptr;

use ghostty_vt_sys as sys;

use crate::error::{Result, check};
use crate::io;
use crate::terminal::{ScrollbackRow, Term};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Plain,
    Vt(VtExtras),
    Html,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors GhosttyFormatterScreenExtra and GhosttyFormatterTerminalExtra"
)]
pub struct VtExtras {
    pub cursor: bool,
    pub style: bool,
    pub hyperlink: bool,
    pub modes: bool,
    pub keyboard: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub format: Format,
    pub unwrap: bool,
    pub trim: bool,
    pub region: Region,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Region {
    #[default]
    All,
    FromRow(ScrollbackRow),
    Clip {
        screen_row: u16,
        cols: u16,
        rows: u16,
    },
}

pub fn format(term: &Term, options: Options) -> Result<Vec<u8>> {
    let extras = match options.format {
        Format::Vt(extras) => extras,
        Format::Plain | Format::Html => VtExtras::default(),
    };
    let mut raw_options = sys::GhosttyFormatterTerminalOptions {
        size: size_of::<sys::GhosttyFormatterTerminalOptions>(),
        emit: match options.format {
            Format::Plain => sys::GHOSTTY_FORMATTER_FORMAT_PLAIN,
            Format::Vt(_) => sys::GHOSTTY_FORMATTER_FORMAT_VT,
            Format::Html => sys::GHOSTTY_FORMATTER_FORMAT_HTML,
        },
        unwrap: options.unwrap,
        trim: options.trim,
        extra: sys::GhosttyFormatterTerminalExtra {
            size: size_of::<sys::GhosttyFormatterTerminalExtra>(),
            palette: false,
            modes: extras.modes,
            scrolling_region: false,
            tabstops: false,
            pwd: false,
            keyboard: extras.keyboard,
            screen: sys::GhosttyFormatterScreenExtra {
                size: size_of::<sys::GhosttyFormatterScreenExtra>(),
                cursor: extras.cursor,
                style: extras.style,
                hyperlink: extras.hyperlink,
                protection: false,
                kitty_keyboard: extras.keyboard,
                charsets: false,
            },
        },
        selection: ptr::null(),
    };
    let active = sys::GHOSTTY_POINT_TAG_ACTIVE;
    let selection = match options.region {
        Region::All => None,
        Region::FromRow(ScrollbackRow(row)) => Some(selection(
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
            screen_row,
            cols,
            rows,
        } => {
            let top = screen_row.min(term.rows().saturating_sub(1));
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
        sys::ghostty_formatter_terminal_new(
            ptr::null(),
            &raw mut formatter,
            term.raw(),
            raw_options,
        )
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
    check(unsafe { sys::ghostty_terminal_grid_ref(term.raw(), point, &raw mut out) })?;
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
