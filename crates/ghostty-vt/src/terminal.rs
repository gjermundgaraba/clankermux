use std::ffi::c_void;
use std::num::NonZeroU32;
use std::ptr;
use std::sync::Arc;

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

impl ColorScheme {
    fn raw(self) -> sys::GhosttyColorScheme {
        match self {
            ColorScheme::Light => sys::GHOSTTY_COLOR_SCHEME_LIGHT,
            ColorScheme::Dark => sys::GHOSTTY_COLOR_SCHEME_DARK,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScrollbackRow(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    pub col: u16,
    pub row: u16,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SizeReport {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: Option<NonZeroU32>,
    pub cell_height: Option<NonZeroU32>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    pub state: ProgramStatusState,
    pub kind: ProgramStatusKind,
    pub progress: Option<u8>,
    pub id: String,
    pub app: String,
    pub title: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    Remove,
    Set,
    Error,
    Indeterminate,
    Pause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticPrompt {
    PromptStart(PromptKind),
    InputStart,
    OutputStart,
    CommandEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Primary,
    Right,
    Continuation,
    Secondary,
}

/// Effects raised by the engine while it parses, the ones clankermux
/// acts on; the rest (clipboard, resize requests) are left unset at the
/// engine, which ignores them. Every method has a default that
/// ignores the effect. The engine calls these from `extern "C"`
/// trampolines, so a panic in a handler aborts the process.
pub trait Handler: Send + 'static {
    /// Bytes the terminal wants written to the PTY: query answers and mode
    /// reports.
    fn write_pty(&mut self, _data: &[u8]) {}
    fn program_status(&mut self, _report: ProgramStatus) {}
    fn semantic_prompt(&mut self, _prompt: SemanticPrompt) {}
    /// OSC 133 `D`, after `semantic_prompt` with `CommandEnd`: the exit
    /// code, when the shell reported one.
    fn command_end(&mut self, _exit: Option<i32>) {}
    fn bell(&mut self) {}
    /// OSC 9 or OSC 777; the title is empty when the sequence has none.
    fn desktop_notification(&mut self, _title: String, _body: String) {}
    fn progress_report(&mut self, _state: ProgressState, _progress: Option<u8>) {}
    /// Answer to CSI ? 996 n; `None` leaves the query unanswered.
    fn color_scheme(&mut self) -> Option<ColorScheme> {
        None
    }
    /// Answer to XTWINOPS size queries and mode 2048 reports.
    fn size(&mut self) -> Option<SizeReport> {
        None
    }
    /// The title is new; read it from the terminal after the callback.
    fn title_changed(&mut self) {}
    fn pwd_changed(&mut self) {}
    /// A full reset (RIS): the title and pwd are cleared without the two
    /// callbacks above.
    fn reset(&mut self) {}
}

impl Handler for () {}

pub fn color_scheme_report(scheme: ColorScheme) -> Vec<u8> {
    let mut buf = [0u8; 32];
    let mut written = 0;
    // SAFETY: buf holds buf.len() writable bytes; the report is short.
    let result = unsafe {
        sys::ghostty_color_scheme_report_encode(
            scheme.raw(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &raw mut written,
        )
    };
    debug_assert_eq!(check(result), Ok::<(), Error>(()));
    buf[..written].to_vec()
}

struct Slot<H> {
    handler: H,
    xtversion: Vec<u8>,
}

pub struct Term(sys::GhosttyTerminal);

impl Term {
    pub(crate) fn raw(&self) -> sys::GhosttyTerminal {
        self.0
    }

    fn get<T: Default>(&self, key: sys::GhosttyTerminalData) -> Result<T> {
        let mut out = T::default();
        // SAFETY: T matches the documented output type of key at each call site.
        check(unsafe { sys::ghostty_terminal_get(self.0, key, (&raw mut out).cast::<c_void>()) })?;
        Ok(out)
    }

    pub fn cols(&self) -> u16 {
        self.get(sys::GHOSTTY_TERMINAL_DATA_COLS).unwrap_or(0)
    }

    pub fn rows(&self) -> u16 {
        self.get(sys::GHOSTTY_TERMINAL_DATA_ROWS).unwrap_or(0)
    }

    pub fn cursor(&self) -> CursorPosition {
        CursorPosition {
            col: self.get(sys::GHOSTTY_TERMINAL_DATA_CURSOR_X).unwrap_or(0),
            row: self.get(sys::GHOSTTY_TERMINAL_DATA_CURSOR_Y).unwrap_or(0),
        }
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
        unsafe { string(&s) }
    }

    pub fn scrollback_rows(&self) -> Result<u32> {
        let rows: usize = self.get(sys::GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS)?;
        u32::try_from(rows).map_err(|_| Error::LimitExceeded)
    }

    pub fn alternate_screen(&self) -> bool {
        self.get::<sys::GhosttyTerminalScreen>(sys::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN)
            .is_ok_and(|screen| screen == sys::GHOSTTY_TERMINAL_SCREEN_ALTERNATE)
    }

    pub fn screen_top(&self) -> Result<ScrollbackRow> {
        self.scrollback_rows().map(ScrollbackRow)
    }

    pub(crate) fn row_continues(&self, ScrollbackRow(y): ScrollbackRow) -> Result<bool> {
        let point = sys::GhosttyPoint {
            tag: sys::GHOSTTY_POINT_TAG_SCREEN,
            value: sys::GhosttyPointValue {
                coordinate: sys::GhosttyPointCoordinate { x: 0, y },
            },
        };
        let mut grid = sys::GhosttyGridRef {
            size: size_of::<sys::GhosttyGridRef>(),
            ..Default::default()
        };
        let mut row = sys::GhosttyRow::default();
        let mut out = false;
        // SAFETY: the terminal is live, the reference is used before any
        // change, and the output type of WRAP_CONTINUATION is bool.
        unsafe {
            check(sys::ghostty_terminal_grid_ref(self.0, point, &raw mut grid))?;
            check(sys::ghostty_grid_ref_row(&raw const grid, &raw mut row))?;
            check(sys::ghostty_row_get(
                row,
                sys::GHOSTTY_ROW_DATA_WRAP_CONTINUATION,
                (&raw mut out).cast::<c_void>(),
            ))?;
        }
        Ok(out)
    }

    pub fn history_start(&self, lines: usize) -> Result<ScrollbackRow> {
        let ScrollbackRow(mut row) = self.screen_top()?;
        while row > 0 && self.row_continues(ScrollbackRow(row))? {
            row -= 1;
        }
        let mut counted = 0;
        while row > 0 && counted < lines {
            row -= 1;
            if !self.row_continues(ScrollbackRow(row))? {
                counted += 1;
            }
        }
        Ok(ScrollbackRow(row))
    }

    pub fn mode(&self, mode: u16) -> Result<bool> {
        let mut config = sys::GhosttyTerminalModeConfig { mode, value: false };
        // SAFETY: output type is GhosttyTerminalModeConfig* with mode set.
        check(unsafe {
            sys::ghostty_terminal_get(
                self.0,
                sys::GHOSTTY_TERMINAL_DATA_MODE,
                (&raw mut config).cast::<c_void>(),
            )
        })?;
        Ok(config.value)
    }
}

pub struct Terminal<H: Handler = ()> {
    term: Term,
    owner: Arc<Owner<H>>,
}

/// What frees the terminal and the slot its callbacks reach through
/// userdata (a raw pointer so that Rust-side access and the callbacks
/// share one provenance).
struct Owner<H: Handler> {
    raw: sys::GhosttyTerminal,
    slot: *mut Slot<H>,
}

impl<H: Handler> Drop for Owner<H> {
    fn drop(&mut self) {
        // SAFETY: the terminal was created by the library and is freed once,
        // before the slot its callbacks point at, which Box::into_raw made.
        unsafe {
            sys::ghostty_terminal_free(self.raw);
            drop(Box::from_raw(self.slot));
        }
    }
}

// SAFETY: libghostty-vt keeps no thread-local state, so a terminal may be
// used from any thread as long as calls are serialized, which `&mut`
// access guarantees. The handler is `Send` by its trait bound.
unsafe impl<H: Handler> Send for Terminal<H> {}

impl<H: Handler> std::ops::Deref for Terminal<H> {
    type Target = Term;
    fn deref(&self) -> &Term {
        &self.term
    }
}

impl Terminal<()> {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        Self::with_handler(cols, rows, ())
    }
}

impl<H: Handler> Terminal<H> {
    pub fn with_handler(cols: u16, rows: u16, handler: H) -> Result<Self> {
        let mut raw = ptr::null_mut();
        // SAFETY: a null allocator selects the default allocator.
        check(unsafe { sys::ghostty_terminal_new(ptr::null(), &raw mut raw, cols, rows) })?;
        // SAFETY: raw is a fresh caller-owned terminal.
        Ok(unsafe { Terminal::install(raw, handler) })
    }

    pub(crate) unsafe fn install(raw: sys::GhosttyTerminal, handler: H) -> Self {
        let slot = Box::into_raw(Box::new(Slot {
            handler,
            xtversion: Vec::new(),
        }));
        let callbacks: [(sys::GhosttyTerminalOption, *const c_void); 12] = [
            (sys::GHOSTTY_TERMINAL_OPT_WRITE_PTY, cb::write_pty::<H> as _),
            (sys::GHOSTTY_TERMINAL_OPT_BELL, cb::bell::<H> as _),
            (
                sys::GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION,
                cb::desktop_notification::<H> as _,
            ),
            (
                sys::GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT,
                cb::progress_report::<H> as _,
            ),
            (
                sys::GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS,
                cb::program_status::<H> as _,
            ),
            (
                sys::GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT,
                cb::semantic_prompt::<H> as _,
            ),
            (
                sys::GHOSTTY_TERMINAL_OPT_COLOR_SCHEME,
                cb::color_scheme::<H> as _,
            ),
            (sys::GHOSTTY_TERMINAL_OPT_SIZE, cb::size::<H> as _),
            (
                sys::GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                cb::title_changed::<H> as _,
            ),
            (
                sys::GHOSTTY_TERMINAL_OPT_PWD_CHANGED,
                cb::pwd_changed::<H> as _,
            ),
            (sys::GHOSTTY_TERMINAL_OPT_RESET, cb::reset::<H> as _),
            // An empty answer reports the engine's default.
            (sys::GHOSTTY_TERMINAL_OPT_XTVERSION, cb::xtversion::<H> as _),
        ];
        // SAFETY: the slot lives as long as the terminal, and each callback
        // pointer has the type its option documents; callback options take
        // the function pointer itself as the value.
        unsafe {
            sys::ghostty_terminal_set(
                raw,
                sys::GHOSTTY_TERMINAL_OPT_USERDATA,
                slot.cast::<c_void>(),
            );
            for (option, f) in callbacks {
                let result = sys::ghostty_terminal_set(raw, option, f);
                debug_assert_eq!(check(result), Ok::<(), Error>(()));
            }
        }
        Terminal {
            term: Term(raw),
            owner: Arc::new(Owner { raw, slot }),
        }
    }

    pub(crate) fn share(&self) -> Self {
        Terminal {
            term: Term(self.term.0),
            owner: self.owner.clone(),
        }
    }

    fn slot(&self) -> &Slot<H> {
        // SAFETY: the slot lives as long as the owner, and the library only
        // reaches it during calls that take `&mut self`.
        unsafe { &*self.owner.slot }
    }

    fn slot_mut(&mut self) -> &mut Slot<H> {
        unsafe { &mut *self.owner.slot }
    }

    pub fn handler(&self) -> &H {
        &self.slot().handler
    }

    pub fn handler_mut(&mut self) -> &mut H {
        &mut self.slot_mut().handler
    }

    /// Answer to XTVERSION; empty for the engine's default.
    pub fn set_xtversion(&mut self, version: &str) {
        self.slot_mut().xtversion = version.as_bytes().to_vec();
    }

    /// The terminfo entry reported for XTGETTCAP `TN`; at most 128 bytes.
    pub fn set_terminfo_name(&mut self, name: &str) -> Result<()> {
        let value = sys::GhosttyString {
            ptr: name.as_ptr(),
            len: name.len(),
        };
        // The option copies the string during the call.
        self.set(
            sys::GHOSTTY_TERMINAL_OPT_TERMINFO_NAME,
            (&raw const value).cast::<c_void>(),
        )
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
        let value = raw
            .as_ref()
            .map_or(ptr::null(), |c| ptr::from_ref(c).cast());
        self.set(option, value)
    }

    pub fn set_scrollback_max_bytes(&mut self, bytes: usize) -> Result<()> {
        self.set(
            sys::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
            (&raw const bytes).cast::<c_void>(),
        )
    }

    pub fn set_continuation_max_bytes(&mut self, bytes: usize) -> Result<()> {
        self.set(
            sys::GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES,
            (&raw const bytes).cast::<c_void>(),
        )
    }
}

mod cb {
    use super::{
        Handler, NonZeroU32, ProgramStatus, ProgramStatusKind, ProgramStatusState, ProgressState,
        PromptKind, SemanticPrompt, Slot, c_void, string, sys,
    };

    unsafe fn slot<'a, H: Handler>(userdata: *mut c_void) -> &'a mut Slot<H> {
        // SAFETY: userdata is the Slot<H> installed by Terminal::install.
        unsafe { &mut *userdata.cast::<Slot<H>>() }
    }

    pub(super) unsafe extern "C" fn write_pty<H: Handler>(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        data: *const u8,
        len: usize,
    ) {
        // SAFETY: data points at len bytes for this call.
        let bytes = unsafe { crate::io::bytes(data, len) };
        unsafe { slot::<H>(ud) }.handler.write_pty(bytes);
    }

    pub(super) unsafe extern "C" fn program_status<H: Handler>(
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
                progress: u8::try_from(r.progress).ok(),
                id: string(&r.id),
                app: string(&r.app),
                title: string(&r.title),
                message: string(&r.message),
            }
        };
        unsafe { slot::<H>(ud) }.handler.program_status(status);
    }

    pub(super) unsafe extern "C" fn semantic_prompt<H: Handler>(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        event: *const sys::GhosttyTerminalSemanticPrompt,
    ) {
        // SAFETY: event is valid for this call.
        let e = unsafe { &*event };
        let mark = match e.kind {
            sys::GHOSTTY_SEMANTIC_PROMPT_PROMPT_START => {
                SemanticPrompt::PromptStart(match e.prompt_kind {
                    sys::GHOSTTY_SEMANTIC_PROMPT_PROMPT_RIGHT => PromptKind::Right,
                    sys::GHOSTTY_SEMANTIC_PROMPT_PROMPT_CONTINUATION => PromptKind::Continuation,
                    sys::GHOSTTY_SEMANTIC_PROMPT_PROMPT_SECONDARY => PromptKind::Secondary,
                    _ => PromptKind::Primary,
                })
            }
            sys::GHOSTTY_SEMANTIC_PROMPT_INPUT_START => SemanticPrompt::InputStart,
            sys::GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START => SemanticPrompt::OutputStart,
            sys::GHOSTTY_SEMANTIC_PROMPT_COMMAND_END => SemanticPrompt::CommandEnd,
            _ => return,
        };
        let handler = &mut unsafe { slot::<H>(ud) }.handler;
        handler.semantic_prompt(mark);
        if mark == SemanticPrompt::CommandEnd {
            handler.command_end(e.has_exit_code.then_some(e.exit_code));
        }
    }

    pub(super) unsafe extern "C" fn bell<H: Handler>(_t: sys::GhosttyTerminal, ud: *mut c_void) {
        unsafe { slot::<H>(ud) }.handler.bell();
    }

    pub(super) unsafe extern "C" fn desktop_notification<H: Handler>(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        notification: *const sys::GhosttyTerminalDesktopNotification,
    ) {
        // SAFETY: the notification and its strings are valid for this call.
        let n = unsafe { &*notification };
        let (title, body) = unsafe { (string(&n.title), string(&n.body)) };
        unsafe { slot::<H>(ud) }
            .handler
            .desktop_notification(title, body);
    }

    pub(super) unsafe extern "C" fn progress_report<H: Handler>(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        report: *const sys::GhosttyTerminalProgressReport,
    ) {
        // SAFETY: report is valid for this call.
        let r = unsafe { &*report };
        let state = match r.state {
            sys::GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE => ProgressState::Remove,
            sys::GHOSTTY_TERMINAL_PROGRESS_STATE_SET => ProgressState::Set,
            sys::GHOSTTY_TERMINAL_PROGRESS_STATE_ERROR => ProgressState::Error,
            sys::GHOSTTY_TERMINAL_PROGRESS_STATE_INDETERMINATE => ProgressState::Indeterminate,
            sys::GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE => ProgressState::Pause,
            _ => return,
        };
        unsafe { slot::<H>(ud) }
            .handler
            .progress_report(state, u8::try_from(r.progress).ok());
    }

    pub(super) unsafe extern "C" fn color_scheme<H: Handler>(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        out: *mut sys::GhosttyColorScheme,
    ) -> bool {
        match unsafe { slot::<H>(ud) }.handler.color_scheme() {
            Some(scheme) => {
                // SAFETY: out is a valid output pointer for this call.
                unsafe { *out = scheme.raw() };
                true
            }
            None => false,
        }
    }

    pub(super) unsafe extern "C" fn size<H: Handler>(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
        out: *mut sys::GhosttySizeReportSize,
    ) -> bool {
        match unsafe { slot::<H>(ud) }.handler.size() {
            Some(size) => {
                // SAFETY: out is a valid output pointer for this call.
                unsafe {
                    *out = sys::GhosttySizeReportSize {
                        rows: size.rows,
                        columns: size.cols,
                        cell_width: size.cell_width.map_or(0, NonZeroU32::get),
                        cell_height: size.cell_height.map_or(0, NonZeroU32::get),
                    }
                };
                true
            }
            None => false,
        }
    }

    pub(super) unsafe extern "C" fn title_changed<H: Handler>(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
    ) {
        unsafe { slot::<H>(ud) }.handler.title_changed();
    }

    pub(super) unsafe extern "C" fn pwd_changed<H: Handler>(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
    ) {
        unsafe { slot::<H>(ud) }.handler.pwd_changed();
    }

    pub(super) unsafe extern "C" fn reset<H: Handler>(_t: sys::GhosttyTerminal, ud: *mut c_void) {
        unsafe { slot::<H>(ud) }.handler.reset();
    }

    pub(super) unsafe extern "C" fn xtversion<H: Handler>(
        _t: sys::GhosttyTerminal,
        ud: *mut c_void,
    ) -> sys::GhosttyString {
        let v = &unsafe { slot::<H>(ud) }.xtversion;
        sys::GhosttyString {
            ptr: v.as_ptr(),
            len: v.len(),
        }
    }
}
