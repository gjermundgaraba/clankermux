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
    reason = "one flag per group of GhosttyFormatterExtras bits"
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
    let emit = match options.format {
        Format::Plain => sys::GHOSTTY_FORMATTER_FORMAT_PLAIN,
        Format::Vt(_) => sys::GHOSTTY_FORMATTER_FORMAT_VT,
        Format::Html => sys::GHOSTTY_FORMATTER_FORMAT_HTML,
    };
    let keyboard =
        sys::GHOSTTY_FORMATTER_EXTRA_KEYBOARD | sys::GHOSTTY_FORMATTER_EXTRA_KITTY_KEYBOARD;
    let raw_extras: sys::GhosttyFormatterExtras = [
        (extras.modes, sys::GHOSTTY_FORMATTER_EXTRA_MODES),
        (extras.keyboard, keyboard),
        (extras.cursor, sys::GHOSTTY_FORMATTER_EXTRA_CURSOR),
        (extras.style, sys::GHOSTTY_FORMATTER_EXTRA_STYLE),
        (extras.hyperlink, sys::GHOSTTY_FORMATTER_EXTRA_HYPERLINK),
    ]
    .into_iter()
    .filter_map(|(on, bits)| on.then_some(bits))
    .fold(sys::GHOSTTY_FORMATTER_EXTRAS_NONE, |all, bits| all | bits);
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
    let mut formatter = ptr::null_mut();
    // SAFETY: term outlives the formatter, which is freed before returning.
    check(unsafe {
        sys::ghostty_formatter_terminal_new(ptr::null(), &raw mut formatter, term.raw())
    })?;
    let set = |option, value: *const std::ffi::c_void| {
        // SAFETY: the formatter is live and each value has the option's
        // type; the formatter copies it during the call.
        check(unsafe { sys::ghostty_formatter_set(formatter, option, value) })
    };
    let mut out = Vec::new();
    let result = set(sys::GHOSTTY_FORMATTER_OPT_EMIT, (&raw const emit).cast())
        .and_then(|()| {
            set(
                sys::GHOSTTY_FORMATTER_OPT_UNWRAP,
                (&raw const options.unwrap).cast(),
            )
        })
        .and_then(|()| {
            set(
                sys::GHOSTTY_FORMATTER_OPT_TRIM,
                (&raw const options.trim).cast(),
            )
        })
        .and_then(|()| {
            set(
                sys::GHOSTTY_FORMATTER_OPT_EXTRAS,
                (&raw const raw_extras).cast(),
            )
        })
        .and_then(|()| match &selection {
            Some(selection) => set(
                sys::GHOSTTY_FORMATTER_OPT_SELECTION,
                ptr::from_ref(selection).cast(),
            ),
            None => Ok(()),
        })
        // SAFETY: the writer borrows `out` only for the duration of this call.
        .and_then(|()| {
            check(unsafe { sys::ghostty_formatter_format(formatter, io::writer(&mut out)) })
        });
    unsafe { sys::ghostty_formatter_free(formatter) };
    result?;
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
    let mut out = sys::GhosttyGridRef::default();
    // SAFETY: term is live and out is a GhosttyGridRef.
    check(unsafe { sys::ghostty_terminal_grid_ref(term.raw(), point, &raw mut out) })?;
    Ok(out)
}

fn selection(
    start: sys::GhosttyGridRef,
    end: sys::GhosttyGridRef,
    rectangle: bool,
) -> sys::GhosttySelection {
    sys::GhosttySelection {
        start,
        end,
        rectangle,
    }
}
