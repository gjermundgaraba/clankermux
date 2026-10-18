//! One session: the child, its PTY, the engine and the stream.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ghostty_vt::{
    ColorKind, ColorScheme, Handler, Rgb, SizeReport, Term, Terminal, color_scheme_report,
};
use proto::{ClosedReason, Event, Exit, OnExit, Profile, Scheme, Size, Status, Viewport};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::registry::Registry;
use crate::stream::{Marker, MarkerKind, Stream};

pub type Shared = Arc<Mutex<Session>>;

/// Coalescing window for `VIEWPORT` reports.
pub const COALESCE: Duration = Duration::from_millis(25);

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

    fn title_changed(&mut self, _term: &Term) {
        self.changed = true;
    }

    fn pwd_changed(&mut self, _term: &Term) {
        self.changed = true;
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

/// `#rrggbb`.
pub fn parse_rgb(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
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

/// A viewport report from an attachment, or from `set_size` with no source.
#[derive(Debug, Clone)]
pub struct Report {
    pub source: Option<u64>,
    pub viewport: Viewport,
}

/// An open attachment as the session sees it.
pub struct Attachment {
    pub id: u64,
    /// Tells the attachment to send `CLOSED` and end. Weak, so that the
    /// connection dropping its handle ends the attachment.
    pub close: mpsc::WeakUnboundedSender<ClosedReason>,
    /// Set once it has sent the final marker and, if bootstrapping,
    /// `SNAPSHOT_END`.
    pub finished: Arc<AtomicBool>,
}

pub struct Session {
    pub id: String,
    pub label: String,
    pub status: Status,
    pub argv: Vec<String>,
    pub cwd: String,
    /// The child's pid while it is alive.
    pub pid: Option<u32>,
    /// The child's process group, which may outlive the child.
    pub pgid: u32,
    pub on_exit: OnExit,
    pub created_at: String,
    pub exited_at: Option<String>,
    /// Set once the child is reaped.
    pub exit: Option<Exit>,
    pub term: Terminal<Effects>,
    pub stream: Stream,
    /// Notified after every stream change.
    pub wake: Arc<Notify>,
    pub master: Arc<AsyncFd<OwnedFd>>,
    pub slave_path: String,
    pub input: InputQueue,
    pub pty_eof: bool,
    /// `limits.text`, applied to the title and pwd the program reports.
    pub text_limit: usize,
    pub attached: Vec<Attachment>,
    /// The attachment whose report was adopted last.
    pub authoritative: Option<u64>,
    pub profile: Profile,
    /// The latest coalesced report, waiting for the timer.
    pub pending: Option<Report>,
    pub timer_running: bool,
}

impl Session {
    pub fn to_proto(&self, with_records: bool) -> proto::Session {
        proto::Session {
            id: self.id.clone(),
            label: self.label.clone(),
            status: self.status,
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            pid: self.pid,
            cols: self.term.cols(),
            rows: self.term.rows(),
            title: truncate(self.term.title(), self.text_limit),
            pwd: truncate(pwd_path(&self.term.pwd()), self.text_limit),
            on_exit: self.on_exit,
            created_at: self.created_at.clone(),
            exited_at: self.exited_at.clone(),
            exit: self.exit.clone(),
            offset: self.stream.offset(),
            marker: self.stream.latest_seq(),
            attachments: self.attached.len() as u32,
            program_status: with_records.then(Vec::new),
        }
    }

    /// Feeds PTY bytes to the engine and the stream. Returns whether the
    /// session's reported fields changed.
    pub fn output(&mut self, bytes: &[u8]) -> bool {
        self.term.vt_write(bytes);
        self.stream.append(bytes);
        self.wake.notify_waiters();
        self.flush_effects()
    }

    /// Writes the engine's pending answers to the PTY and reports whether
    /// title or pwd changed.
    pub fn flush_effects(&mut self) -> bool {
        let effects = self.term.handler_mut();
        if !effects.pty_output.is_empty() {
            let answers = std::mem::take(&mut effects.pty_output);
            if self.input.push(answers).is_none() {
                tracing::warn!(session = self.id, "input queue full; query answers dropped");
            }
        }
        std::mem::take(&mut self.term.handler_mut().changed)
    }

    pub fn size(&self) -> Size {
        Size {
            cols: self.term.cols(),
            rows: self.term.rows(),
            cell_width: None,
            cell_height: None,
        }
    }

    /// Applies a viewport report at once, discarding any pending coalesced
    /// report. Returns the marker when the size changed.
    pub fn apply_report(&mut self, report: Report) -> Option<Marker> {
        self.pending = None;
        if self.status != Status::Running {
            return None;
        }
        if let Some(source) = report.source {
            self.authoritative = Some(source);
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
        } = report.viewport;
        if let Some(profile) = profile {
            self.set_profile(profile);
        }
        let current = self.term.handler().size;
        let cell_width = cell_width.unwrap_or(current.cell_width);
        let cell_height = cell_height.unwrap_or(current.cell_height);
        let changed = (self.term.cols(), self.term.rows()) != (cols, rows);
        if changed
            && self
                .term
                .resize(cols, rows, cell_width, cell_height)
                .is_err()
        {
            return None;
        }
        self.term.handler_mut().size = SizeReport {
            cols,
            rows,
            cell_width,
            cell_height,
        };
        if !changed {
            return None;
        }
        let _ = crate::pty::set_size(&self.master, cols, rows);
        self.flush_effects();
        let marker = self.stream.add_marker(MarkerKind::Resized { cols, rows });
        self.wake.notify_waiters();
        Some(marker)
    }

    /// Makes `profile` the authoritative one: default colours for OSC 10,
    /// 11 and 12, the scheme for CSI ? 996 n, and a mode 2031 report when
    /// the scheme answer changed.
    pub fn set_profile(&mut self, profile: Profile) {
        let before = self.term.handler().scheme_answer();
        let effects = self.term.handler_mut();
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
            let _ = self
                .term
                .set_color(kind, value.as_deref().and_then(parse_rgb));
        }
        self.profile = profile;
        let after = self.term.handler().scheme_answer();
        if after != before
            && let Some(scheme) = after
            && self.term.mode(2031).unwrap_or(false)
            && self.input.push(color_scheme_report(scheme)).is_none()
        {
            tracing::warn!(session = self.id, "input queue full; scheme report dropped");
        }
    }

    /// Called when the PTY reached EOF or the child was reaped; ends the
    /// stream once both have happened. Returns the exit when it did.
    pub fn maybe_end(&mut self) -> Option<Exit> {
        if !self.pty_eof || self.status != Status::Running {
            return None;
        }
        let exit = self.exit.clone()?;
        self.status = Status::Exited;
        self.exited_at = Some(now());
        self.stream
            .add_marker(MarkerKind::Exited { exit: exit.clone() });
        self.wake.notify_waiters();
        Some(exit)
    }

    /// Whether every open attachment has finished.
    pub fn attachments_finished(&self) -> bool {
        self.attached
            .iter()
            .all(|a| a.finished.load(Ordering::Acquire))
    }

    /// Tells attachments to close: all of them, or only the finished ones.
    pub fn close_attachments(&self, reason: ClosedReason, finished_only: bool) {
        for a in &self.attached {
            if (!finished_only || a.finished.load(Ordering::Acquire))
                && let Some(close) = a.close.upgrade()
            {
                let _ = close.send(reason);
            }
        }
    }
}

/// Applies a viewport report, at once or after the coalescing window, and
/// emits `session_changed` when the size changed.
pub fn report(registry: &Arc<Registry>, session: &Shared, report: Report, immediate: bool) {
    let mut s = session.lock().unwrap();
    if immediate {
        let changed = s.apply_report(report).map(|_| s.to_proto(false));
        drop(s);
        if let Some(session) = changed {
            registry.emit(&Event::SessionChanged { session });
        }
        return;
    }
    s.pending = Some(report);
    if s.timer_running {
        return;
    }
    s.timer_running = true;
    drop(s);
    let registry = registry.clone();
    let session = session.clone();
    tokio::spawn(async move {
        tokio::time::sleep(COALESCE).await;
        let changed = {
            let mut s = session.lock().unwrap();
            s.timer_running = false;
            s.pending
                .take()
                .and_then(|r| s.apply_report(r))
                .map(|_| s.to_proto(false))
        };
        if let Some(session) = changed {
            registry.emit(&Event::SessionChanged { session });
        }
    });
}

/// The path of an OSC 7 `file://host/path` value; other values unchanged.
pub fn pwd_path(value: &str) -> String {
    match value.strip_prefix("file://") {
        Some(rest) => rest.find('/').map_or("", |i| &rest[i..]).to_string(),
        None => value.to_string(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pwd_strips_scheme_and_host() {
        assert_eq!(pwd_path("file://mac.local/Users/me"), "/Users/me");
        assert_eq!(pwd_path("file:///tmp"), "/tmp");
        assert_eq!(pwd_path("/plain"), "/plain");
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
