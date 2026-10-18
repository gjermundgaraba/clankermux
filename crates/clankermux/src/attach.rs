//! The terminal side of `attach`: raw mode, the outer size, passthrough
//! or repaint, keys, the detach prefix, and reconnecting with resume.

use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitCode;
use std::time::Duration;

use client::connection::hello;
use client::expect::Expect;
use client::vt::{self, Scan};
use client::{Action, Attachment, Connection, Error, Frame};
use ghostty_vt::formatter::{self, Format, Region};
use proto::{ClosedReason, Exit, FrameType, Open, Size, Viewport};
use rustix::termios::{self, OptionalActions, Termios};
use tokio::io::AsyncReadExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::Instant;

pub struct Options {
    pub socket: PathBuf,
    pub session: String,
    pub read_only: bool,
    /// The prefix byte; followed by `d` it detaches, twice it is sent.
    pub detach_key: u8,
}

/// `C-x` or `^x` for a control key: a letter, or one of `@ [ \ ] ^ _ ?`.
pub fn parse_key(spec: &str) -> Option<u8> {
    let key = match spec.as_bytes() {
        [b'C', b'-', c] | [b'^', c] => *c,
        _ => return None,
    };
    (key.is_ascii_alphabetic() || b"@[\\]^_?".contains(&key))
        .then(|| key.to_ascii_uppercase() ^ 0x40)
}

/// The codepoint the kitty keyboard protocol reports with `ctrl` for the
/// key that produces control byte `byte`.
fn kitty_codepoint(byte: u8) -> u32 {
    u32::from(match byte {
        1..=26 => byte + 96,
        0x1b..=0x1f => byte + 64,
        0x7f => b'?',
        0 => b' ',
        other => other,
    })
}

/// How long to wait for the rest of an escape sequence from stdin.
const KEY_TIMEOUT: Duration = Duration::from_millis(25);
/// Coalescing window for repaints while the sizes differ.
const REPAINT: Duration = Duration::from_millis(16);
const CHANNEL: u32 = 1;

/// Leaves every mode passthrough or a paint may have set: attributes,
/// cursor hidden, main screen, mouse off (and its encodings), bracketed
/// paste and focus off, modifyOtherKeys and kitty keys off, cursor keys
/// and keypad normal, no scrolling region, origin mode off, autowrap on,
/// default cursor style, synchronized output off.
const RESET_MODES: &[u8] = b"\x1b[0m\x1b[?25l\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1015l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[>4;0m\x1b[<u\x1b[=0;1u\x1b[?1l\x1b>\x1b[r\x1b[?6l\x1b[?7h\x1b[0 q\x1b[?2026l";
/// The reset written on exit: the modes, then the cursor shown.
const RESET: &[u8] = b"\x1b[0m\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1015l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[>4;0m\x1b[<u\x1b[=0;1u\x1b[?1l\x1b>\x1b[r\x1b[?6l\x1b[?7h\x1b[0 q\x1b[?2026l\x1b[?25h";

/// Raw mode on stdin, restored on drop.
struct Raw {
    original: Termios,
}

impl Raw {
    fn enter() -> std::io::Result<Self> {
        let stdin = std::io::stdin();
        let original = termios::tcgetattr(&stdin)?;
        let mut raw = original.clone();
        raw.make_raw();
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw)?;
        Ok(Raw { original })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(std::io::stdin(), OptionalActions::Now, &self.original);
    }
}

fn outer_size() -> Size {
    let ws = termios::tcgetwinsize(std::io::stdout()).ok();
    let (cols, rows) = ws.map_or((80, 24), |w| (w.ws_col.max(1), w.ws_row.max(1)));
    let cell = |pixels: u16, cells: u16| (pixels > 0).then(|| u32::from(pixels / cells));
    Size {
        cols,
        rows,
        cell_width: ws.and_then(|w| cell(w.ws_xpixel, cols)),
        cell_height: ws.and_then(|w| cell(w.ws_ypixel, rows)),
    }
}

fn write_out(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

struct Shell {
    opts: Options,
    attachment: Attachment<Expect>,
    outer: Size,
    /// A repaint is due by then.
    repaint_at: Option<Instant>,
    /// Stdin bytes that do not yet form whole key encodings, and when to
    /// stop waiting for the rest.
    pending: Vec<u8>,
    keys_at: Option<Instant>,
    /// The detach prefix was pressed: its encoding, sent on if what
    /// follows is not `d`.
    armed: Option<Vec<u8>>,
}

/// Why the attach ended.
enum Outcome {
    Detached,
    Exited(Exit),
    Closed(ClosedReason),
    Failed(String),
}

/// What a batch of keys asks for.
enum Keys {
    Send(Vec<u8>),
    Detach,
}

impl Shell {
    /// Output goes straight to the outer terminal while its size equals
    /// the session's; otherwise the screen is repainted from the engine.
    /// The engine's handler expects answers only in passthrough.
    fn passthrough(&mut self) -> bool {
        let outer = self.outer;
        match self.attachment.term() {
            Some(term) => {
                let same = (term.cols(), term.rows()) == (outer.cols, outer.rows);
                term.handler_mut().passthrough = same;
                same
            }
            None => false,
        }
    }

    /// Paints the screen (never the history) clipped to the outer
    /// terminal: the bottom rows, or when the cursor is above them the
    /// rows ending at the cursor.
    fn paint(&mut self) {
        self.repaint_at = None;
        let outer = self.outer;
        let Some(term) = self.attachment.term() else {
            return;
        };
        let (cols, rows) = (term.cols(), term.rows());
        let (cursor_col, cursor_row) = term.cursor();
        let clip = (outer.cols.min(cols), outer.rows.min(rows));
        let from_row = (rows - clip.1).min((cursor_row + 1).saturating_sub(clip.1));
        let painted = formatter::format(
            term,
            formatter::Options {
                format: Format::Vt,
                cursor: true,
                style: true,
                hyperlink: true,
                modes: true,
                scrolling_region: false,
                keyboard: true,
                region: Region::Clip {
                    from_row,
                    cols: clip.0,
                    rows: clip.1,
                },
                ..Default::default()
            },
        );
        let visible = term.mode(25).unwrap_or(true) && cursor_col < clip.0;
        let mut out = RESET_MODES.to_vec();
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        if let Ok(painted) = painted {
            out.extend_from_slice(&painted);
        }
        // The formatter places the cursor where the session has it; the
        // clip moves it.
        out.extend_from_slice(
            format!("\x1b[{};{}H", cursor_row - from_row + 1, cursor_col + 1).as_bytes(),
        );
        if visible {
            out.extend_from_slice(b"\x1b[?25h");
        }
        write_out(&out);
    }

    fn apply(&mut self, action: Action) -> Option<Outcome> {
        match action {
            Action::Screen => {
                self.passthrough();
                self.paint();
                // A session whose stream ended sends no EXITED.
                if let Some(exit) = self
                    .attachment
                    .session
                    .as_ref()
                    .and_then(|s| s.exit.clone())
                {
                    return Some(Outcome::Exited(exit));
                }
            }
            Action::Output(bytes) => {
                if self.passthrough() {
                    write_out(&bytes);
                } else if self.repaint_at.is_none() {
                    self.repaint_at = Some(Instant::now() + REPAINT);
                }
            }
            Action::Resized { .. } => {
                self.passthrough();
                self.paint();
            }
            Action::Gap(_) => {}
            Action::Exited(exit) => return Some(Outcome::Exited(exit)),
            Action::Closed(reason) => return Some(Outcome::Closed(reason)),
            Action::Error(error) => {
                if self.attachment.ended() {
                    return Some(Outcome::Failed(Error::Server(error).to_string()));
                }
                write_out(b"\x07");
            }
        }
        None
    }

    fn is_prefix(&self, unit: &[u8]) -> bool {
        unit == [self.opts.detach_key]
            || vt::kitty_key(unit) == Some((kitty_codepoint(self.opts.detach_key), 2))
    }

    /// Takes the whole keys from `pending` (all of it with `flush`), drops
    /// the outer terminal's answers to forwarded queries, and handles the
    /// detach prefix.
    fn keys(&mut self, flush: bool) -> Keys {
        let strings = self
            .attachment
            .term()
            .is_some_and(|t| t.handler_mut().expects_string());
        let complete = if flush {
            self.pending.len()
        } else {
            vt::complete_prefix(&self.pending, strings)
        };
        let bytes: Vec<u8> = self.pending.drain(..complete).collect();
        self.keys_at = (!self.pending.is_empty()).then(|| Instant::now() + KEY_TIMEOUT);
        let bytes = match self.attachment.term() {
            Some(term) => term.handler_mut().filter(&bytes),
            None => bytes,
        };
        let mut out = Vec::with_capacity(bytes.len());
        let mut pos = 0;
        while pos < bytes.len() {
            let len = match vt::scan(&bytes[pos..], strings) {
                Scan::Sequence(n) => n,
                Scan::Plain(_) => 1,
                Scan::Incomplete => bytes.len() - pos,
            };
            let unit = &bytes[pos..pos + len];
            pos += len;
            match self.armed.take() {
                Some(prefix) => {
                    if unit == b"d" || vt::kitty_key(unit) == Some((u32::from(b'd'), 0)) {
                        return Keys::Detach;
                    }
                    if !self.is_prefix(unit) {
                        out.extend_from_slice(&prefix);
                    }
                    out.extend_from_slice(unit);
                }
                None if self.is_prefix(unit) => self.armed = Some(unit.to_vec()),
                None => out.extend_from_slice(unit),
            }
        }
        Keys::Send(out)
    }
}

type Connecting = Pin<Box<dyn Future<Output = Connection> + Send>>;

/// Retries with backoff until the server answers.
fn reconnect(socket: PathBuf) -> Connecting {
    Box::pin(async move {
        let mut backoff = Duration::from_millis(250);
        loop {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
            if let Ok(conn) = connect(&socket).await {
                return conn;
            }
        }
    })
}

async fn connect(socket: &std::path::Path) -> Result<Connection, Error> {
    Connection::connect_unix(socket, hello("clankermux", Some(ghostty_vt::ENGINE))).await
}

/// The connection and the channel's frames, or the reconnect in progress.
enum Link {
    Up {
        conn: Box<Connection>,
        frames: mpsc::UnboundedReceiver<Frame>,
    },
    Down(Connecting),
}

enum LinkEvent {
    Frame(Frame),
    /// The connection ended, for this reason.
    Lost(Option<Error>),
    Connected(Box<Connection>),
}

impl Link {
    async fn next(&mut self) -> LinkEvent {
        match self {
            Link::Up { conn, frames } => match frames.recv().await {
                Some(frame) => LinkEvent::Frame(frame),
                None => LinkEvent::Lost(conn.ended()),
            },
            Link::Down(connecting) => LinkEvent::Connected(Box::new(connecting.await)),
        }
    }
}

fn open(shell: &mut Shell, conn: &Connection) -> Result<mpsc::UnboundedReceiver<Frame>, Error> {
    shell.attachment.open_on(&conn.welcome);
    conn.open(
        CHANNEL,
        &Open {
            session: shell.opts.session.clone(),
            read_only: shell.opts.read_only,
            viewport: (!shell.opts.read_only).then_some(shell.outer),
            profile: None,
            resume: shell.attachment.resume(),
        },
    )
}

pub async fn run(opts: Options) -> Result<ExitCode, Error> {
    let conn = connect(&opts.socket).await?;
    let raw = Raw::enter().map_err(Error::Io)?;
    let mut shell = Shell {
        attachment: Attachment::new(),
        opts,
        outer: outer_size(),
        repaint_at: None,
        pending: Vec::new(),
        keys_at: None,
        armed: None,
    };
    let mut link = match open(&mut shell, &conn) {
        Ok(frames) => Link::Up {
            conn: Box::new(conn),
            frames,
        },
        Err(err) => {
            drop(raw);
            return Err(err);
        }
    };
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 4096];
    let mut winch = signal(SignalKind::window_change()).map_err(Error::Io)?;
    let mut term_signal = signal(SignalKind::terminate()).map_err(Error::Io)?;
    let mut int_signal = signal(SignalKind::interrupt()).map_err(Error::Io)?;
    let mut hup_signal = signal(SignalKind::hangup()).map_err(Error::Io)?;
    let mut quit_signal = signal(SignalKind::quit()).map_err(Error::Io)?;

    let outcome = loop {
        let far = Instant::now() + Duration::from_secs(3600);
        let keys_at = shell.keys_at.unwrap_or(far);
        let repaint_at = shell.repaint_at.unwrap_or(far);
        let keys = tokio::select! {
            event = link.next() => match event {
                LinkEvent::Frame(frame) => {
                    let actions = match shell.attachment.handle(frame) {
                        Ok(actions) => actions,
                        Err(err) => break Outcome::Failed(err.to_string()),
                    };
                    if let Some(outcome) = actions.into_iter().find_map(|a| shell.apply(a)) {
                        break outcome;
                    }
                    continue;
                }
                LinkEvent::Lost(Some(Error::Server(error))) => {
                    break Outcome::Failed(Error::Server(error).to_string());
                }
                LinkEvent::Lost(_) => {
                    link = Link::Down(reconnect(shell.opts.socket.clone()));
                    continue;
                }
                LinkEvent::Connected(conn) => {
                    // A `snapshot` answer repaints; `resume` continues.
                    link = match open(&mut shell, &conn) {
                        Ok(frames) => Link::Up { conn, frames },
                        Err(_) => Link::Down(reconnect(shell.opts.socket.clone())),
                    };
                    continue;
                }
            },
            read = stdin.read(&mut buf) => match read {
                Ok(0) | Err(_) => break Outcome::Detached,
                Ok(n) => {
                    shell.pending.extend_from_slice(&buf[..n]);
                    shell.keys(false)
                }
            },
            _ = tokio::time::sleep_until(keys_at) => shell.keys(true),
            _ = tokio::time::sleep_until(repaint_at) => {
                shell.paint();
                continue;
            },
            _ = winch.recv() => {
                shell.outer = outer_size();
                if let (false, Link::Up { conn, .. }) = (shell.opts.read_only, &link) {
                    let _ = conn.send_json(FrameType::Viewport, CHANNEL, &Viewport { size: shell.outer, profile: None });
                }
                // Painted even when the sizes now match: the screen must
                // be whole before passthrough continues on it.
                shell.passthrough();
                shell.paint();
                continue;
            },
            _ = term_signal.recv() => break Outcome::Detached,
            _ = int_signal.recv() => break Outcome::Detached,
            _ = hup_signal.recv() => break Outcome::Detached,
            _ = quit_signal.recv() => break Outcome::Detached,
        };
        match keys {
            Keys::Detach => break Outcome::Detached,
            Keys::Send(bytes) => {
                if let (false, false, Link::Up { conn, .. }) =
                    (bytes.is_empty(), shell.opts.read_only, &link)
                {
                    // A failure here shows as the end of the frames.
                    let _ = conn.send(FrameType::Input, CHANNEL, &bytes);
                }
            }
        }
    };
    write_out(RESET);
    drop(raw);
    let (message, code) = match outcome {
        Outcome::Detached => ("detached".to_string(), ExitCode::SUCCESS),
        Outcome::Exited(Exit::Code { code }) => (
            format!("session exited with code {code}"),
            if code == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            },
        ),
        Outcome::Exited(Exit::Signal { signal }) => {
            (format!("session killed by SIG{signal}"), ExitCode::FAILURE)
        }
        Outcome::Closed(ClosedReason::Removed) => {
            ("session removed".to_string(), ExitCode::SUCCESS)
        }
        Outcome::Closed(reason) => (
            format!("closed: {reason:?}").to_lowercase(),
            ExitCode::SUCCESS,
        ),
        Outcome::Failed(message) => (message, ExitCode::FAILURE),
    };
    eprintln!("\r\n[clankermux: {message}]");
    Ok(code)
}
