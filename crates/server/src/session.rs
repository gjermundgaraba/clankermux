//! One session: the child, its PTY, the engine and the stream.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ghostty_vt::{
    ColorKind, ColorScheme, Handler, ProgramStatus, Rgb, SemanticPromptKind, SizeReport, Terminal,
    color_scheme_report, formatter, snapshot,
};
use proto::{
    ClosedReason, ErrorCode, Event, Exit, OnExit, Profile, Scheme, Size, Status, Viewport,
};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::attach::Ending;
use crate::park::ParkFile;
use crate::records::Records;
use crate::registry::{Fail, Registry};
use crate::stream::{Marker, MarkerKind, Stream};

pub type Shared = Arc<Mutex<Session>>;

/// Coalescing window for `VIEWPORT` reports.
pub const COALESCE: Duration = Duration::from_millis(25);

/// How long a snapshot is retried while the continuation limit is
/// exceeded, for a bootstrap and for a park.
pub const SNAPSHOT_RETRY: Duration = Duration::from_secs(5);

/// Effects the engine raises while parsing, collected under the session
/// lock and drained by the caller of `vt_write`.
#[derive(Default)]
pub struct Effects {
    /// Query answers and reports the terminal wants written to the PTY.
    pub pty_output: Vec<u8>,
    pub changed: bool,
    pub scheme: Option<ColorScheme>,
    pub bg: Option<Rgb>,
    pub size: SizeReport,
    /// OSC 7501 reports and OSC 133 `A` prompt starts, in stream order.
    pub status: Vec<StatusEvent>,
}

pub enum StatusEvent {
    Report(ProgramStatus),
    /// Transient records are dropped.
    PromptStart,
}

impl Effects {
    /// Answer to the colour scheme query: the profile's scheme, else from
    /// its background, else none.
    pub fn scheme_answer(&self) -> Option<ColorScheme> {
        self.scheme.or_else(|| {
            self.bg.map(|c| {
                if luminance(c) < 0.5 {
                    ColorScheme::Dark
                } else {
                    ColorScheme::Light
                }
            })
        })
    }
}

impl Handler for Effects {
    fn write_pty(&mut self, data: &[u8]) {
        self.pty_output.extend_from_slice(data);
    }

    fn color_scheme(&mut self) -> Option<ColorScheme> {
        self.scheme_answer()
    }

    fn size(&mut self) -> Option<SizeReport> {
        Some(self.size)
    }

    fn title_changed(&mut self) {
        self.changed = true;
    }

    fn pwd_changed(&mut self) {
        self.changed = true;
    }

    /// A full reset clears the title and pwd without the two callbacks
    /// above.
    fn reset(&mut self) {
        self.changed = true;
    }

    fn program_status(&mut self, report: ProgramStatus) {
        self.status.push(StatusEvent::Report(report));
    }

    fn semantic_prompt(&mut self, kind: SemanticPromptKind) {
        if kind == SemanticPromptKind::PromptStart {
            self.status.push(StatusEvent::PromptStart);
        }
    }
}

/// Relative luminance per WCAG, 0 for black and 1 for white.
pub fn luminance(c: Rgb) -> f64 {
    fn lin(v: u8) -> f64 {
        let s = v as f64 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// The colours OSC 10, 11 and 12 report when the profile leaves them
/// unset: Ghostty's own defaults, so that programs that query before any
/// client attached still get an answer.
pub fn default_color(kind: ColorKind) -> Rgb {
    match kind {
        ColorKind::Foreground | ColorKind::Cursor => Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },
        ColorKind::Background => Rgb {
            r: 0x28,
            g: 0x2c,
            b: 0x34,
        },
    }
}

/// `#rrggbb`.
pub fn parse_rgb(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Rgb {
        r: channel(0)?,
        g: channel(2)?,
        b: channel(4)?,
    })
}

/// One queued write: its bytes, the queue permits they hold until written,
/// and the sender resolved once they are written.
pub type InputUnit = (Vec<u8>, OwnedSemaphorePermit, oneshot::Sender<()>);

/// Bytes queued for the PTY, bounded by `limits.input_queue`.
pub struct InputQueue {
    tx: mpsc::UnboundedSender<InputUnit>,
    permits: Arc<Semaphore>,
}

impl InputQueue {
    pub fn new(max: usize) -> (Self, mpsc::UnboundedReceiver<InputUnit>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let queue = InputQueue {
            tx,
            permits: Arc::new(Semaphore::new(max)),
        };
        (queue, rx)
    }

    /// Queues `bytes` to be written as one unit. The receiver resolves once
    /// they are written and fails when they could not be. Returns `None`
    /// when the queue would exceed its bound.
    pub fn push(&self, bytes: Vec<u8>) -> Option<oneshot::Receiver<()>> {
        let permit = self
            .permits
            .clone()
            .try_acquire_many_owned(u32::try_from(bytes.len()).ok()?)
            .ok()?;
        let (done, wait) = oneshot::channel();
        self.tx.send((bytes, permit, done)).ok()?;
        Some(wait)
    }
}

/// `cols` and `rows` of a size are at least 1.
pub fn check_size(size: &Size) -> Result<(), crate::registry::Fail> {
    if size.cols == 0 || size.rows == 0 {
        return Err((
            ErrorCode::InvalidRequest,
            "cols and rows must be at least 1".into(),
        ));
    }
    Ok(())
}

/// An open attachment as the session sees it.
pub struct Attachment {
    pub id: u64,
    /// Tells the attachment to send `CLOSED` and end. Weak, so that the
    /// connection dropping its handle ends the attachment.
    pub close: mpsc::WeakUnboundedSender<Ending>,
    /// Set once it has sent the final marker and, if bootstrapping,
    /// `SNAPSHOT_END`.
    pub finished: Arc<AtomicBool>,
}

/// What only a session with a process behind it has.
pub struct Live {
    /// The child's pid while it is alive.
    pub pid: Option<u32>,
    /// The child's process group, which may outlive the child.
    pub pgid: u32,
    pub term: Terminal<Effects>,
    pub stream: Stream,
    pub master: Arc<AsyncFd<OwnedFd>>,
    pub slave_path: String,
    pub input: InputQueue,
    pub pty_eof: bool,
    /// The latest coalesced report, waiting for the timer.
    pub pending: Option<Viewport>,
    pub timer_running: bool,
}

impl Live {
    /// History and screen as text, one logical line each, and how many
    /// of the leading lines are history.
    pub fn text(&self) -> Result<(String, usize), ghostty_vt::Error> {
        let text = |region| {
            formatter::format(
                &self.term,
                formatter::Options {
                    unwrap: true,
                    trim: true,
                    region,
                    ..Default::default()
                },
            )
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };
        let all = text(formatter::Region::FromRow(0))?;
        let screen = text(formatter::Region::FromRow(
            self.term.scrollback_rows() as u32
        ))?;
        let history_lines = all.lines().count() - screen.lines().count();
        Ok((all, history_lines))
    }
}

/// A session loaded from a park file: a screen, no process.
pub struct Parked {
    /// The engine that wrote the snapshot; `OPEN` needs the server's.
    pub engine: String,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub pwd: String,
    pub offset: u64,
    pub marker: u64,
    /// `screen.txt`: history and screen at the park, and how many of its
    /// leading lines are history.
    pub screen: String,
    pub history_lines: usize,
    pub snapshot_path: PathBuf,
}

pub enum Body {
    Live(Box<Live>),
    Parked(Parked),
}

pub struct Session {
    pub id: String,
    pub label: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub on_exit: OnExit,
    pub created_at: String,
    pub exited_at: Option<String>,
    /// Set once the child is reaped, or from the park file.
    pub exit: Option<Exit>,
    /// Notified after every stream change.
    pub wake: Arc<Notify>,
    /// `limits.text`, applied to the title and pwd the program reports.
    pub text_limit: usize,
    /// `limits.records`.
    pub records_limit: usize,
    pub attached: Vec<Attachment>,
    pub records: Records,
    /// Events raised under the lock, emitted by whoever releases it.
    pub events: Vec<Event>,
    /// Being removed: at once, or after the grace while attachments
    /// finish. A park in flight deletes what it wrote; a stop deletes the
    /// files rather than parking.
    pub removed: bool,
    /// The title and pwd last announced by `session_changed`.
    pub announced: (String, String),
    /// Parks of this session run one at a time.
    pub park_lock: Arc<tokio::sync::Mutex<()>>,
    /// The latest `write` reply task; the next replies after it.
    pub write_chain: Option<tokio::task::JoinHandle<()>>,
    /// The last generation written, so the next is larger whatever the
    /// clock says.
    pub last_generation: u64,
    pub body: Body,
}

impl Session {
    /// `running` while live and not ended, `exited` once ended, `restored`
    /// for a parked session that had not.
    pub fn status(&self) -> Status {
        match (&self.body, &self.exited_at) {
            (_, Some(_)) => Status::Exited,
            (Body::Live(_), None) => Status::Running,
            (Body::Parked(_), None) => Status::Restored,
        }
    }

    /// The live body of a running session, else `not_running`.
    pub fn running(&self) -> Result<&Live, Fail> {
        match (&self.body, &self.exited_at) {
            (Body::Live(live), None) => Ok(live),
            _ => Err((ErrorCode::NotRunning, "session is not running".into())),
        }
    }

    /// Whether the session is going: removed, or ended with
    /// `on_exit: remove`. Its files are not worth writing.
    pub fn gone(&self) -> bool {
        self.removed || (self.on_exit == OnExit::Remove && self.exited_at.is_some())
    }

    pub fn live(&self) -> Option<&Live> {
        match &self.body {
            Body::Live(live) => Some(live),
            Body::Parked(_) => None,
        }
    }

    pub fn live_mut(&mut self) -> Option<&mut Live> {
        match &mut self.body {
            Body::Live(live) => Some(live),
            Body::Parked(_) => None,
        }
    }

    pub fn parked(&self) -> Option<&Parked> {
        match &self.body {
            Body::Parked(parked) => Some(parked),
            Body::Live(_) => None,
        }
    }

    pub fn to_proto(&self, with_records: bool) -> proto::Session {
        let (pid, cols, rows, title, pwd, offset, marker) = match &self.body {
            Body::Live(live) => (
                live.pid,
                live.term.cols(),
                live.term.rows(),
                live.term.title(),
                pwd_path(&live.term.pwd()),
                live.stream.offset(),
                live.stream.latest_seq(),
            ),
            Body::Parked(parked) => (
                None,
                parked.cols,
                parked.rows,
                parked.title.clone(),
                parked.pwd.clone(),
                parked.offset,
                parked.marker,
            ),
        };
        proto::Session {
            id: self.id.clone(),
            label: self.label.clone(),
            status: self.status(),
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            pid,
            cols,
            rows,
            title: truncate(title, self.text_limit),
            pwd: truncate(pwd, self.text_limit),
            on_exit: self.on_exit,
            created_at: self.created_at.clone(),
            exited_at: self.exited_at.clone(),
            exit: self.exit.clone(),
            offset,
            marker,
            attachments: self.attached.len() as u32,
            program_status: with_records.then(|| self.records.resolved()),
        }
    }

    /// The events raised since the last take.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Feeds PTY bytes to the engine and the stream; the events this
    /// raises wait in `events`.
    pub fn output(&mut self, bytes: &[u8]) {
        let Body::Live(live) = &mut self.body else {
            return;
        };
        live.term.vt_write(bytes);
        live.stream.append(bytes);
        self.wake.notify_waiters();
        self.flush_effects();
    }

    /// Writes the engine's pending answers to the PTY, applies the
    /// program status reports, and queues `session_changed` when title or
    /// pwd changed.
    pub fn flush_effects(&mut self) {
        let Body::Live(live) = &mut self.body else {
            return;
        };
        let effects = live.term.handler_mut();
        let answers = std::mem::take(&mut effects.pty_output);
        let status = std::mem::take(&mut effects.status);
        let changed = std::mem::take(&mut effects.changed);
        if !answers.is_empty() && live.input.push(answers).is_none() {
            tracing::warn!(session = self.id, "input queue full; query answers dropped");
        }
        let now = now();
        for event in status {
            let change = match event {
                StatusEvent::Report(report) => self.records.apply(report, self.records_limit, &now),
                StatusEvent::PromptStart => {
                    self.drop_transient_records();
                    continue;
                }
            };
            if let Some(record) = change.set {
                self.events.push(Event::ProgramStatusChanged {
                    id: self.id.clone(),
                    record,
                });
            }
            if !change.removed.is_empty() {
                self.events.push(Event::ProgramStatusRemoved {
                    id: self.id.clone(),
                    record_ids: change.removed,
                });
            }
        }
        if changed {
            let session = self.to_proto(false);
            let identity = (session.title.clone(), session.pwd.clone());
            if identity != self.announced {
                self.announced = identity;
                self.events.push(Event::SessionChanged { session });
            }
        }
    }

    fn drop_transient_records(&mut self) {
        let removed = self.records.drop_transient();
        if !removed.is_empty() {
            self.events.push(Event::ProgramStatusRemoved {
                id: self.id.clone(),
                record_ids: removed,
            });
        }
    }

    pub fn size(&self) -> Size {
        let (cols, rows) = match &self.body {
            Body::Live(live) => (live.term.cols(), live.term.rows()),
            Body::Parked(parked) => (parked.cols, parked.rows),
        };
        Size {
            cols,
            rows,
            cell_width: None,
            cell_height: None,
        }
    }

    /// Applies a viewport report at once, discarding any pending coalesced
    /// report. Returns the marker when the size changed.
    pub fn apply_report(&mut self, report: Viewport) -> Option<Marker> {
        let Body::Live(live) = &mut self.body else {
            return None;
        };
        live.pending = None;
        if self.exited_at.is_some() {
            return None;
        }
        let Viewport {
            size:
                Size {
                    cols,
                    rows,
                    cell_width,
                    cell_height,
                },
            profile,
        } = report;
        if let Some(profile) = profile {
            self.set_profile(profile);
        }
        let Body::Live(live) = &mut self.body else {
            return None;
        };
        let current = live.term.handler().size;
        let cell_width = cell_width.unwrap_or(current.cell_width);
        let cell_height = cell_height.unwrap_or(current.cell_height);
        let changed = (live.term.cols(), live.term.rows()) != (cols, rows);
        if changed
            && live
                .term
                .resize(cols, rows, cell_width, cell_height)
                .is_err()
        {
            return None;
        }
        live.term.handler_mut().size = SizeReport {
            cols,
            rows,
            cell_width,
            cell_height,
        };
        if !changed {
            return None;
        }
        let _ = crate::pty::set_size(&live.master, cols, rows);
        let marker = live.stream.add_marker(MarkerKind::Resized { cols, rows });
        self.flush_effects();
        self.wake.notify_waiters();
        Some(marker)
    }

    /// Makes `profile` the session's: default colours for OSC 10, 11 and
    /// 12 (falling back to [`default_color`]), the scheme for CSI ? 996
    /// n, and a mode 2031 report when the scheme answer changed.
    pub fn set_profile(&mut self, profile: Profile) {
        let id = self.id.clone();
        let Body::Live(live) = &mut self.body else {
            return;
        };
        let before = live.term.handler().scheme_answer();
        let effects = live.term.handler_mut();
        effects.scheme = profile.scheme.map(|s| match s {
            Scheme::Light => ColorScheme::Light,
            Scheme::Dark => ColorScheme::Dark,
        });
        effects.bg = profile.bg.as_deref().and_then(parse_rgb);
        for (kind, value) in [
            (ColorKind::Foreground, &profile.fg),
            (ColorKind::Background, &profile.bg),
            (ColorKind::Cursor, &profile.cursor),
        ] {
            let color = value
                .as_deref()
                .and_then(parse_rgb)
                .unwrap_or(default_color(kind));
            let _ = live.term.set_color(kind, Some(color));
        }
        let after = live.term.handler().scheme_answer();
        if after != before
            && let Some(scheme) = after
            && live.term.mode(2031).unwrap_or(false)
            && live.input.push(color_scheme_report(scheme)).is_none()
        {
            tracing::warn!(session = id, "input queue full; scheme report dropped");
        }
    }

    /// Called when the PTY reached EOF or the child was reaped; ends the
    /// stream once both have happened. Returns the exit when it did.
    pub fn maybe_end(&mut self) -> Option<Exit> {
        if self.exited_at.is_some() {
            return None;
        }
        let exit = self.exit.clone()?;
        let Body::Live(live) = &mut self.body else {
            return None;
        };
        if !live.pty_eof {
            return None;
        }
        self.exited_at = Some(now());
        live.stream
            .add_marker(MarkerKind::Exited { exit: exit.clone() });
        self.drop_transient_records();
        self.wake.notify_waiters();
        Some(exit)
    }

    /// The text `read` serves: the screen preceded by the last `scrollback`
    /// lines of history, one logical line each, the same text live or
    /// parked.
    pub fn read_text(&self, scrollback: usize) -> Result<String, ghostty_vt::Error> {
        let (text, history_lines) = match &self.body {
            Body::Live(live) => blocking(|| live.text())?,
            Body::Parked(parked) => (parked.screen.clone(), parked.history_lines),
        };
        let skip = history_lines.saturating_sub(scrollback);
        Ok(text.lines().skip(skip).collect::<Vec<_>>().join("\n"))
    }

    /// The park file and `screen.txt` of a live session, as of `at`.
    pub fn park_file(
        &self,
        live: &Live,
        at: jiff::Timestamp,
    ) -> Result<(ParkFile, String), ghostty_vt::Error> {
        let (all, history_lines) = live.text()?;
        let file = ParkFile {
            protocol: crate::park::PROTOCOL,
            engine: ghostty_vt::ENGINE.into(),
            parked_at: at.to_string(),
            session: proto::Session {
                attachments: 0,
                pid: None,
                program_status: Some(self.records.all()),
                ..self.to_proto(false)
            },
            history_lines: history_lines as u64,
        };
        Ok((file, all))
    }

    /// Whether every open attachment has finished.
    pub fn attachments_finished(&self) -> bool {
        self.attached
            .iter()
            .all(|a| a.finished.load(Ordering::Acquire))
    }

    /// Sends `CLOSED reason` to every attachment, or only the finished
    /// ones.
    pub fn close_attachments(&self, reason: ClosedReason, finished_only: bool) {
        for a in &self.attached {
            if (!finished_only || a.finished.load(Ordering::Acquire))
                && let Some(close) = a.close.upgrade()
            {
                let _ = close.send(Ending::Closed(reason));
            }
        }
    }
}

/// Applies a viewport report, at once or after the coalescing window, and
/// emits `session_changed` when the size changed.
pub fn report(registry: &Arc<Registry>, session: &Shared, report: Viewport, immediate: bool) {
    let mut s = session.lock().unwrap();
    if immediate {
        let changed = s.apply_report(report).map(|_| s.to_proto(false));
        let events = s.take_events();
        drop(s);
        registry.emit_session(events, changed);
        return;
    }
    let Some(live) = s.live_mut() else {
        return;
    };
    // The latest dimensions, and the profile and cell size of the latest
    // report that carried them.
    live.pending = Some(match live.pending.take() {
        Some(earlier) => Viewport {
            size: Size {
                cell_width: report.size.cell_width.or(earlier.size.cell_width),
                cell_height: report.size.cell_height.or(earlier.size.cell_height),
                ..report.size
            },
            profile: report.profile.or(earlier.profile),
        },
        None => report,
    });
    if live.timer_running {
        return;
    }
    live.timer_running = true;
    drop(s);
    let registry = registry.clone();
    let session = session.clone();
    tokio::spawn(async move {
        tokio::time::sleep(COALESCE).await;
        let (changed, events) = {
            let mut s = session.lock().unwrap();
            let pending = s.live_mut().and_then(|live| {
                live.timer_running = false;
                live.pending.take()
            });
            let changed = pending
                .and_then(|r| s.apply_report(r))
                .map(|_| s.to_proto(false));
            (changed, s.take_events())
        };
        registry.emit_session(events, changed);
    });
}

/// The path of an OSC 7 value: what follows the host of any
/// `scheme://host/path` (`file://` from most shells, `kitty-shell-cwd://`
/// from Ghostty's integration), percent-decoded; a bare path as is.
pub fn pwd_path(value: &str) -> String {
    let path = match value.split_once("://") {
        // `file://host/path`, or `kitty-shell-cwd://host/path` from
        // Ghostty's shell integration: the path, percent-decoded.
        Some((_, rest)) => rest.find('/').map_or("", |i| &rest[i..]),
        None => value,
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = match bytes.get(i + 1..i + 3) {
            Some(hex) if bytes[i] == b'%' => std::str::from_utf8(hex)
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok()),
            _ => None,
        };
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first at most `max` bytes of `text`, on a character boundary.
pub fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

pub fn now() -> String {
    jiff::Timestamp::now().to_string()
}

/// Runs engine work that holds the session still; on a multi-thread
/// runtime the worker is released meanwhile.
pub fn blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::current().runtime_flavor() {
        tokio::runtime::RuntimeFlavor::CurrentThread => work(),
        _ => tokio::task::block_in_place(work),
    }
}

/// Encodes a live session's snapshot, retrying for up to `retry` while
/// the continuation limit is exceeded, and runs `take` with the bytes
/// under the session lock. `Ok(None)` when the session is not live.
pub async fn snapshot_with_retry<T>(
    session: &Shared,
    retry: Duration,
    mut take: impl FnMut(&Session, &Live, Vec<u8>) -> T,
) -> Result<Option<T>, ghostty_vt::Error> {
    let deadline = tokio::time::Instant::now() + retry;
    let wake = session.lock().unwrap().wake.clone();
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let taken = {
            let s = session.lock().unwrap();
            let Some(live) = s.live() else {
                return Ok(None);
            };
            blocking(|| snapshot::encode(&live.term).map(|bytes| take(&s, live, bytes)))
        };
        match taken {
            Ok(value) => return Ok(Some(value)),
            Err(ghostty_vt::Error::LimitExceeded) if tokio::time::Instant::now() < deadline => {
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep_until(deadline) => {}
                }
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pwd_strips_scheme_and_host() {
        assert_eq!(pwd_path("file://mac.local/Users/me"), "/Users/me");
        assert_eq!(pwd_path("file:///tmp"), "/tmp");
        assert_eq!(pwd_path("/plain"), "/plain");
        assert_eq!(
            pwd_path("kitty-shell-cwd://mac.local/Users/me/a%20b"),
            "/Users/me/a b"
        );
        assert_eq!(pwd_path("file:///x%2"), "/x%2");
    }

    #[test]
    fn truncates_on_character_boundaries() {
        assert_eq!(truncate("héllo".into(), 2), "h");
        assert_eq!(truncate("héllo".into(), 3), "hé");
        assert_eq!(truncate("hi".into(), 10), "hi");
    }

    #[test]
    fn luminance_splits_dark_and_light() {
        assert!(luminance(Rgb { r: 0, g: 0, b: 0 }) < 0.5);
        assert!(
            luminance(Rgb {
                r: 255,
                g: 255,
                b: 255
            }) > 0.5
        );
        assert_eq!(
            parse_rgb("#1e1e2e"),
            Some(Rgb {
                r: 0x1e,
                g: 0x1e,
                b: 0x2e
            })
        );
        assert_eq!(parse_rgb("1e1e2e"), None);
    }
}
