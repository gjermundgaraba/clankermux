use std::ffi::c_void;
use std::ptr;

use ghostty_vt_sys as sys;

use crate::error::{Error, Result, check};
use crate::io::string;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorKind {
    Foreground,
    Background,
    Cursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Primary,
    Alternate,
}

/// Answer to a size query: the grid size in cells and the cell size in
/// pixels, zero when unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeReport {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u32,
    pub cell_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusKind {
    None,
    Permission,
    Question,
    Auth,
}

/// One OSC 7501 report, as parsed by the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    pub state: ProgramStatusState,
    pub kind: ProgramStatusKind,
    /// -1 when absent.
    pub progress: i8,
    pub id: String,
    pub app: String,
    pub title: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticPromptKind {
    PromptStart,
    InputStart,
    OutputStart,
    CommandEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticPrompt {
    pub kind: SemanticPromptKind,
    pub exit_code: Option<i32>,
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardLocation {
    Standard,
    Selection,
    Primary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardContent {
    pub mime: String,
    pub data: Vec<u8>,
}

/// Effects raised by the engine while it parses. Every method has a no-op
/// default.
pub trait Handler: 'static {
    /// Bytes the terminal wants written to the PTY: query answers and mode
    /// reports.
    fn write_pty(&mut self, _data: &[u8]) {}
    fn program_status(&mut self, _report: ProgramStatus) {}
    fn semantic_prompt(&mut self, _event: SemanticPrompt) {}
    /// Answer to CSI ? 996 n; `None` leaves the query unanswered.
    fn color_scheme(&mut self) -> Option<ColorScheme> {
        None
    }
    /// Answer to XTWINOPS size queries and mode 2048 reports.
    fn size(&mut self) -> Option<SizeReport> {
        None
    }
    /// OSC 52 write. An empty `contents` clears the clipboard.
    fn clipboard_write(&mut self, _location: ClipboardLocation, _contents: &[ClipboardContent]) {}
    /// CSI 8 t request to resize the grid.
    fn resize_window(&mut self, _cols: u16, _rows: u16) {}
    fn title_changed(&mut self, _term: &Term) {}
    fn pwd_changed(&mut self, _term: &Term) {}
    fn bell(&mut self) {}
}

impl Handler for () {}

struct Slot {
    handler: Box<dyn Handler>,
    xtversion: Vec<u8>,
}

/// A borrowed terminal handle with read access. [`Terminal`] derefs to it.
#[repr(transparent)]
pub struct Term(sys::GhosttyTerminal);

impl Term {
    pub(crate) fn raw(&self) -> sys::GhosttyTerminal {
        self.0
    }

    fn get<T: Default>(&self, key: sys::GhosttyTerminalData) -> Result<T> {
        let mut out = T::default();
        // SAFETY: T matches the documented output type of key at each call site.
        check(unsafe {
            sys::ghostty_terminal_get(self.0, key, &mut out as *mut T as *mut c_void)
        })?;
        Ok(out)
    }

    pub fn cols(&self) -> u16 {
        self.get(sys::GHOSTTY_TERMINAL_DATA_COLS).unwrap_or(0)
    }

    pub fn rows(&self) -> u16 {
        self.get(sys::GHOSTTY_TERMINAL_DATA_ROWS).unwrap_or(0)
    }

    /// Cursor column and row, zero-based, within the active area.
    pub fn cursor(&self) -> (u16, u16) {
        (
            self.get(sys::GHOSTTY_TERMINAL_DATA_CURSOR_X).unwrap_or(0),
            self.get(sys::GHOSTTY_TERMINAL_DATA_CURSOR_Y).unwrap_or(0),
        )
    }

    pub fn title(&self) -> String {
        let s: sys::GhosttyString = self
            .get(sys::GHOSTTY_TERMINAL_DATA_TITLE)
            .unwrap_or_default();
        // SAFETY: the borrowed string is valid until the next mutating call,
        // and we copy it out immediately.
        unsafe { string(&s) }
    }

    pub fn pwd(&self) -> String {
        let s: sys::GhosttyString = self.get(sys::GHOSTTY_TERMINAL_DATA_PWD).unwrap_or_default();
        // SAFETY: as in `title`.
        unsafe { string(&s) }
    }

    pub fn active_screen(&self) -> Screen {
        match self.get::<sys::GhosttyTerminalScreen>(sys::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN) {
            Ok(sys::GHOSTTY_TERMINAL_SCREEN_ALTERNATE) => Screen::Alternate,
            _ => Screen::Primary,
        }
    }

    pub fn scrollback_rows(&self) -> usize {
        self.get(sys::GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS)
            .unwrap_or(0)
    }

    /// Whether the VT parser is in the ground state.
    pub fn vt_ground(&self) -> bool {
        self.get(sys::GHOSTTY_TERMINAL_DATA_VT_GROUND)
            .unwrap_or(true)
    }

    /// Current value of a DEC private mode such as 2031 or 2048.
    pub fn mode(&self, mode: u16) -> Result<bool> {
        let mut config = sys::GhosttyTerminalModeConfig { mode, value: false };
        // SAFETY: output type is GhosttyTerminalModeConfig* with mode set.
        check(unsafe {
            sys::ghostty_terminal_get(
                self.0,
                sys::GHOSTTY_TERMINAL_DATA_MODE,
                &mut config as *mut sys::GhosttyTerminalModeConfig as *mut c_void,
            )
        })?;
        Ok(config.value)
    }

    /// Effective colour: the OSC override if any, else the default, else `None`.
    pub fn color(&self, kind: ColorKind) -> Option<Rgb> {
        let key = match kind {
            ColorKind::Foreground => sys::GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND,
            ColorKind::Background => sys::GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND,
            ColorKind::Cursor => sys::GHOSTTY_TERMINAL_DATA_COLOR_CURSOR,
        };
        let c: sys::GhosttyColorRgb = self.get(key).ok()?;
        Some(Rgb {
            r: c.r,
            g: c.g,
            b: c.b,
        })
    }
}

/// An owned terminal with its effect handler.
pub struct Terminal {
    term: Term,
    slot: Box<Slot>,
}

impl std::ops::Deref for Terminal {
    type Target = Term;
    fn deref(&self) -> &Term {
        &self.term
    }
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        let mut raw = ptr::null_mut();
        // SAFETY: a null allocator selects the default allocator.
        check(unsafe { sys::ghostty_terminal_new(ptr::null(), &mut raw, cols, rows) })?;
        // SAFETY: raw is a fresh caller-owned terminal.
        Ok(unsafe { Self::from_raw(raw) })
    }

    /// Wraps a caller-owned raw terminal and installs the effect callbacks.
    pub(crate) unsafe fn from_raw(raw: sys::GhosttyTerminal) -> Self {
        let mut slot = Box::new(Slot {
            handler: Box::new(()),
            xtversion: Vec::new(),
        });
        let userdata = &mut *slot as *mut Slot as *mut c_void;
        // SAFETY: the slot lives as long as the terminal, and each callback
        // pointer has the type the option documents.
        unsafe {
            sys::ghostty_terminal_set(raw, sys::GHOSTTY_TERMINAL_OPT_USERDATA, userdata);
            set_fn(
                raw,
                sys::GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                cb::write_pty as *const (),
            );
            set_fn(
                raw,
                sys::GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS,
                cb::program_status as *const (),
            );
            set_fn(
                raw,
                sys::GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT,
                cb::semantic_prompt as *const (),
            );
            set_fn(
                raw,
                sys::GHOSTTY_TERMINAL_OPT_COLOR_SCHEME,
                cb::color_scheme as *const (),
            );
            set_fn(raw, sys::GHOSTTY_TERMINAL_OPT_SIZE, cb::size as *const ());
            set_fn(
                raw,
                sys::GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                cb::clipboard_write as *const (),
            );
            set_fn(
                raw,
                sys::GHOSTTY_TERMINAL_OPT_RESIZE_WINDOW,
                cb::resize_window as *const (),
            );
            set_fn(
                raw,
                sys::GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                cb::title_changed as *const (),
            );
            set_fn(
                raw,
                sys::GHOSTTY_TERMINAL_OPT_PWD_CHANGED,
                cb::pwd_changed as *const (),
            );
            set_fn(raw, sys::GHOSTTY_TERMINAL_OPT_BELL, cb::bell as *const ());
        }
        Terminal {
            term: Term(raw),
            slot,
        }
    }

    pub fn set_handler(&mut self, handler: Box<dyn Handler>) {
        self.slot.handler = handler;
    }

    pub fn handler_mut(&mut self) -> &mut dyn Handler {
        &mut *self.slot.handler
    }

    /// Answer to XTVERSION.
    pub fn set_xtversion(&mut self, version: &str) {
        self.slot.xtversion = version.as_bytes().to_vec();
        // SAFETY: the callback reads the slot's xtversion, which the slot owns.
        unsafe {
            set_fn(
                self.term.0,
                sys::GHOSTTY_TERMINAL_OPT_XTVERSION,
                cb::xtversion as *const (),
            )
        };
    }

    pub fn vt_write(&mut self, data: &[u8]) {
        // SAFETY: data is a valid slice for the duration of the call.
        unsafe { sys::ghostty_terminal_vt_write(self.term.0, data.as_ptr(), data.len()) }
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<()> {
        // SAFETY: raw is a live terminal.
        check(unsafe {
            sys::ghostty_terminal_resize(self.term.0, cols, rows, cell_width, cell_height)
        })
    }

    fn set(&mut self, option: sys::GhosttyTerminalOption, value: *const c_void) -> Result<()> {
        // SAFETY: value has the documented input type at each call site.
        check(unsafe { sys::ghostty_terminal_set(self.term.0, option, value) })
    }

    /// Sets or clears a default colour.
    pub fn set_color(&mut self, kind: ColorKind, color: Option<Rgb>) -> Result<()> {
        let option = match kind {
            ColorKind::Foreground => sys::GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
            ColorKind::Background => sys::GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
            ColorKind::Cursor => sys::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
        };
        let raw = color.map(|c| sys::GhosttyColorRgb {
            r: c.r,
            g: c.g,
            b: c.b,
        });
        let value = raw.as_ref().map_or(ptr::null(), |c| {
            c as *const sys::GhosttyColorRgb as *const c_void
        });
        self.set(option, value)
    }

    pub fn set_scrollback_max_bytes(&mut self, bytes: usize) -> Result<()> {
        self.set(
            sys::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
            &bytes as *const usize as *const c_void,
        )
    }

    pub fn set_continuation_max_bytes(&mut self, bytes: usize) -> Result<()> {
        self.set(
            sys::GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES,
            &bytes as *const usize as *const c_void,
        )
    }

    pub fn set_mode(&mut self, mode: u16, value: bool) -> Result<()> {
        let config = sys::GhosttyTerminalModeConfig { mode, value };
        self.set(
            sys::GHOSTTY_TERMINAL_OPT_MODE,
            &config as *const sys::GhosttyTerminalModeConfig as *const c_void,
        )
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: the terminal was created by the library and is freed once,
        // before the slot its callbacks point at.
        unsafe { sys::ghostty_terminal_free(self.term.0) }
    }
}

unsafe fn set_fn(raw: sys::GhosttyTerminal, option: sys::GhosttyTerminalOption, f: *const ()) {
    // SAFETY: callback options take the function pointer itself as the value.
    let result = unsafe { sys::ghostty_terminal_set(raw, option, f as *const c_void) };
    debug_assert_eq!(check(result), Ok::<(), Error>(()));
}

mod cb {
    use super::*;

    unsafe fn slot<'a>(userdata: *mut c_void) -> &'a mut Slot {
        // SAFETY: userdata is the Slot installed by Terminal::from_raw.
        unsafe { &mut *(userdata as *mut Slot) }
    }

    pub(super) unsafe extern "C" fn write_pty(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        data: *const u8,
        len: usize,
    ) {
        // SAFETY: data points at len bytes for this call.
        let bytes = unsafe { std::slice::from_raw_parts(data, len) };
        unsafe { slot(ud) }.handler.write_pty(bytes);
    }

    pub(super) unsafe extern "C" fn program_status(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        report: *const sys::GhosttyTerminalProgramStatus,
    ) {
        // SAFETY: report is valid for this call.
        let r = unsafe { &*report };
        let state = match r.state {
            sys::GHOSTTY_PROGRAM_STATUS_STATE_IDLE => ProgramStatusState::Idle,
            sys::GHOSTTY_PROGRAM_STATUS_STATE_WORKING => ProgramStatusState::Working,
            sys::GHOSTTY_PROGRAM_STATUS_STATE_DONE => ProgramStatusState::Done,
            sys::GHOSTTY_PROGRAM_STATUS_STATE_BLOCKED => ProgramStatusState::Blocked,
            sys::GHOSTTY_PROGRAM_STATUS_STATE_ERROR => ProgramStatusState::Error,
            _ => ProgramStatusState::Clear,
        };
        let kind = match r.kind {
            sys::GHOSTTY_PROGRAM_STATUS_KIND_PERMISSION => ProgramStatusKind::Permission,
            sys::GHOSTTY_PROGRAM_STATUS_KIND_QUESTION => ProgramStatusKind::Question,
            sys::GHOSTTY_PROGRAM_STATUS_KIND_AUTH => ProgramStatusKind::Auth,
            _ => ProgramStatusKind::None,
        };
        // SAFETY: the strings are valid for this call.
        let status = unsafe {
            ProgramStatus {
                state,
                kind,
                progress: r.progress,
                id: string(&r.id),
                app: string(&r.app),
                title: string(&r.title),
                message: string(&r.message),
            }
        };
        unsafe { slot(ud) }.handler.program_status(status);
    }

    pub(super) unsafe extern "C" fn semantic_prompt(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        event: *const sys::GhosttyTerminalSemanticPrompt,
    ) {
        // SAFETY: event is valid for this call.
        let e = unsafe { &*event };
        let kind = match e.kind {
            sys::GHOSTTY_SEMANTIC_PROMPT_PROMPT_START => SemanticPromptKind::PromptStart,
            sys::GHOSTTY_SEMANTIC_PROMPT_INPUT_START => SemanticPromptKind::InputStart,
            sys::GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START => SemanticPromptKind::OutputStart,
            sys::GHOSTTY_SEMANTIC_PROMPT_COMMAND_END => SemanticPromptKind::CommandEnd,
            _ => return,
        };
        let prompt = SemanticPrompt {
            kind,
            exit_code: e.has_exit_code.then_some(e.exit_code),
            // SAFETY: the string is valid for this call.
            command: unsafe { string(&e.command) },
        };
        unsafe { slot(ud) }.handler.semantic_prompt(prompt);
    }

    pub(super) unsafe extern "C" fn color_scheme(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        out: *mut sys::GhosttyColorScheme,
    ) -> bool {
        match unsafe { slot(ud) }.handler.color_scheme() {
            Some(scheme) => {
                // SAFETY: out is a valid output pointer for this call.
                unsafe {
                    *out = match scheme {
                        ColorScheme::Light => sys::GHOSTTY_COLOR_SCHEME_LIGHT,
                        ColorScheme::Dark => sys::GHOSTTY_COLOR_SCHEME_DARK,
                    }
                };
                true
            }
            None => false,
        }
    }

    pub(super) unsafe extern "C" fn size(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        out: *mut sys::GhosttySizeReportSize,
    ) -> bool {
        match unsafe { slot(ud) }.handler.size() {
            Some(size) => {
                // SAFETY: out is a valid output pointer for this call.
                unsafe {
                    *out = sys::GhosttySizeReportSize {
                        rows: size.rows,
                        columns: size.cols,
                        cell_width: size.cell_width,
                        cell_height: size.cell_height,
                    }
                };
                true
            }
            None => false,
        }
    }

    pub(super) unsafe extern "C" fn clipboard_write(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        write: *const sys::GhosttyClipboardWrite,
    ) {
        // SAFETY: write and its contents array are valid for this call.
        let w = unsafe { &*write };
        let location = match w.location {
            sys::GHOSTTY_CLIPBOARD_LOCATION_SELECTION => ClipboardLocation::Selection,
            sys::GHOSTTY_CLIPBOARD_LOCATION_PRIMARY => ClipboardLocation::Primary,
            _ => ClipboardLocation::Standard,
        };
        let contents: Vec<ClipboardContent> = if w.contents.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(w.contents, w.contents_len) }
                .iter()
                .map(|c| ClipboardContent {
                    // SAFETY: the strings are valid for this call.
                    mime: unsafe { string(&c.mime) },
                    data: if c.data.ptr.is_null() {
                        Vec::new()
                    } else {
                        unsafe { std::slice::from_raw_parts(c.data.ptr, c.data.len) }.to_vec()
                    },
                })
                .collect()
        };
        unsafe { slot(ud) }
            .handler
            .clipboard_write(location, &contents);
        if let Some(reply) = w.reply {
            let answer = sys::GhosttyClipboardWriteReply {
                size: size_of::<sys::GhosttyClipboardWriteReply>(),
                result: sys::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS,
                remember: false,
            };
            // SAFETY: the request requires exactly this reply call during
            // the callback.
            unsafe { reply(write, &answer) };
        }
    }

    pub(super) unsafe extern "C" fn resize_window(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        request: *const sys::GhosttyTerminalResizeWindow,
    ) {
        // SAFETY: request is valid for this call.
        let r = unsafe { &*request };
        if r.tag == sys::GHOSTTY_TERMINAL_RESIZE_WINDOW_GRID {
            // SAFETY: the grid arm is active for this tag.
            let grid = unsafe { r.value.grid };
            unsafe { slot(ud) }
                .handler
                .resize_window(grid.columns, grid.rows);
        }
    }

    pub(super) unsafe extern "C" fn title_changed(t: sys::GhosttyTerminal, ud: *mut c_void) {
        unsafe { slot(ud) }.handler.title_changed(&Term(t));
    }

    pub(super) unsafe extern "C" fn pwd_changed(t: sys::GhosttyTerminal, ud: *mut c_void) {
        unsafe { slot(ud) }.handler.pwd_changed(&Term(t));
    }

    pub(super) unsafe extern "C" fn bell(_t: sys::GhosttyTerminal, ud: *mut c_void) {
        unsafe { slot(ud) }.handler.bell();
    }

    pub(super) unsafe extern "C" fn xtversion(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
    ) -> sys::GhosttyString {
        let v = &unsafe { slot(ud) }.xtversion;
        sys::GhosttyString {
            ptr: v.as_ptr(),
            len: v.len(),
        }
    }
}
