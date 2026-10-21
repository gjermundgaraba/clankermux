//! The terminal side of `attach`: raw mode, the outer size, passthrough
//! or repaint, keys, the detach prefix, and reconnecting with resume.

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::time::Duration;

use client::expect::Expect;
use client::vt::{self, Scan};
use client::{Action, Attachment, Connection, Endpoint, Error, Frame};
use ghostty_vt::formatter::{self, Format, Region};
use proto::{Closed, ClosedReason, Exit, FrameType, Open, Size, Viewport};
use rustix::termios::{self, OptionalActions, Termios};
use tokio::io::AsyncReadExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::Instant;

pub struct Options {
    pub endpoint: Endpoint,
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

/// The kitty keyboard protocol's `ctrl` modifier bit.
const KITTY_CTRL: u32 = 4;

/// The first screen row of a `clip_rows`-tall window into `rows`: the
/// bottom rows, or when the cursor is above them the rows ending at the
/// cursor.
fn clip_window(rows: u16, clip_rows: u16, cursor_row: u16) -> u16 {
    let bottom = rows - clip_rows;
    if cursor_row >= bottom {
        bottom
    } else {
        (cursor_row + 1).saturating_sub(clip_rows)
    }
}

/// How long to wait for the rest of an escape sequence from stdin.
const KEY_TIMEOUT: Duration = Duration::from_millis(25);
/// Coalescing window for repaints while the sizes differ.
const REPAINT: Duration = Duration::from_millis(16);
const CHANNEL: u32 = 1;

/// Leaves every mode passthrough or a paint may have set: attributes,
/// main screen, mouse off (and its encodings), bracketed paste and focus
/// off, modifyOtherKeys and kitty keys off, cursor keys and keypad
/// normal, no scrolling region, origin mode off, autowrap on, default
/// cursor style, synchronized output off. A paint hides the cursor
/// first; the exit shows it after.
const RESET_MODES: &[u8] = b"\x1b[0m\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1015l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[>4;0m\x1b[<u\x1b[=0;1u\x1b[?1l\x1b>\x1b[r\x1b[?6l\x1b[?7h\x1b[0 q\x1b[?2026l";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

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
    /// The detach prefix was pressed: its encoding and any releases
    /// reported since, sent on if what follows is not `d`.
    armed: Option<Vec<u8>>,
    /// The attachment is to be made afresh after this loop pass: a lost
    /// place in the stream or `too_slow`, which the protocol has the
    /// client recover from by attaching again.
    reopen: bool,
}

/// Why the attach ended.
enum Outcome {
    Detached,
    Exited(Exit),
    /// The session was restored from a previous server: a screen, no process.
    Restored,
    Closed(Closed),
    Failed(String),
}

/// What a batch of keys asks for.
enum Keys {
    Send(Vec<u8>),
    Detach,
}

impl Shell {
    fn new(opts: Options, outer: Size) -> Self {
        Shell {
            opts,
            attachment: Attachment::new(),
            outer,
            repaint_at: None,
            pending: Vec::new(),
            keys_at: None,
            armed: None,
            reopen: false,
        }
    }

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
        let from_row = clip_window(rows, clip.1, cursor_row);
        let painted = formatter::format(
            term,
            formatter::Options {
                format: Format::Vt,
                cursor: true,
                style: true,
                hyperlink: true,
                modes: true,
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
        let mut out = [HIDE_CURSOR, RESET_MODES, b"\x1b[H\x1b[2J"].concat();
        if let Ok(painted) = painted {
            out.extend_from_slice(&painted);
        }
        // The formatter places the cursor where the session has it; the
        // clip moves it.
        out.extend_from_slice(
            format!("\x1b[{};{}H", cursor_row - from_row + 1, cursor_col + 1).as_bytes(),
        );
        if visible {
            out.extend_from_slice(SHOW_CURSOR);
        }
        write_out(&out);
    }

    fn apply(&mut self, action: Action) -> Option<Outcome> {
        match action {
            Action::Screen => {
                self.passthrough();
                self.paint();
                // A session whose stream ended sends no EXITED. A running
                // one may carry its child's exit while a descendant holds
                // the PTY; its EXITED comes with the end of the stream.
                match self.attachment.session.as_ref() {
                    Some(s) if s.status == proto::Status::Restored => {
                        return Some(Outcome::Restored);
                    }
                    Some(s) if s.status == proto::Status::Exited => {
                        if let Some(exit) = s.exit.clone() {
                            return Some(Outcome::Exited(exit));
                        }
                    }
                    _ => {}
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
            Action::Exited(exit) => {
                // The final output may still be waiting for its repaint.
                if self.repaint_at.is_some() {
                    self.paint();
                }
                return Some(Outcome::Exited(exit));
            }
            Action::Closed(closed) => return Some(Outcome::Closed(closed)),
            Action::Error(error) => {
                if self.attachment.ended() {
                    if error.code == proto::ErrorCode::TooSlow {
                        self.reopen = true;
                        return None;
                    }
                    return Some(Outcome::Failed(Error::Server(error).to_string()));
                }
                write_out(b"\x07");
            }
        }
        None
    }

    fn is_prefix(&self, unit: &[u8]) -> bool {
        unit == [self.opts.detach_key]
            || vt::kitty_key(unit) == Some((kitty_codepoint(self.opts.detach_key), KITTY_CTRL))
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
        // The wait for the rest of a sequence starts once, with its first
        // byte.
        self.keys_at = if self.pending.is_empty() {
            None
        } else {
            self.keys_at.or_else(|| Some(Instant::now() + KEY_TIMEOUT))
        };
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
                Some(mut prefix) => {
                    // A release (the prefix's own, or a modifier's, when the
                    // program asked for them) is not the next key.
                    if vt::kitty_release(unit) {
                        prefix.extend_from_slice(unit);
                        self.armed = Some(prefix);
                        continue;
                    }
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

type Connecting = Pin<Box<dyn Future<Output = Result<Connection, Error>> + Send>>;

/// Retries with backoff while the server is unreachable; a server that
/// answers and refuses the handshake ends the attachment.
fn reconnect(endpoint: Endpoint) -> Connecting {
    Box::pin(async move {
        let mut backoff = Duration::from_millis(250);
        loop {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
            match crate::connect(&endpoint).await {
                Ok(conn) => return Ok(conn),
                Err(err @ (Error::Server(_) | Error::Protocol(_))) => return Err(err),
                Err(_) => {}
            }
        }
    })
}

/// The connection and the channel's frames, or the reconnect in progress.
enum Link {
    Up {
        conn: Box<Connection>,
        frames: client::Receiver<Frame>,
    },
    Down(Connecting),
}

enum LinkEvent {
    Frame(Frame),
    /// The connection ended, for this reason.
    Lost(Option<Error>),
    Connected(Box<Connection>),
    /// The server answered a reconnect and refused the handshake.
    Refused(Error),
}

impl Link {
    async fn next(&mut self) -> LinkEvent {
        match self {
            Link::Up { conn, frames } => match frames.recv().await {
                Some(frame) => LinkEvent::Frame(frame),
                None => LinkEvent::Lost(conn.ended()),
            },
            Link::Down(connecting) => match connecting.await {
                Ok(conn) => LinkEvent::Connected(Box::new(conn)),
                Err(err) => LinkEvent::Refused(err),
            },
        }
    }
}

fn open(shell: &mut Shell, conn: &Connection) -> Result<client::Receiver<Frame>, Error> {
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

/// Attaches over `conn`, reconnecting to the endpoint when it is lost,
/// and returns the exit status.
pub async fn run(opts: Options, conn: Connection) -> Result<i32, Error> {
    let raw = Raw::enter()?;
    let mut shell = Shell::new(opts, outer_size());
    let frames = open(&mut shell, &conn)?;
    let mut link = Link::Up {
        conn: Box::new(conn),
        frames,
    };
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 4096];
    let mut winch = signal(SignalKind::window_change())?;
    let mut term_signal = signal(SignalKind::terminate())?;
    let mut int_signal = signal(SignalKind::interrupt())?;
    let mut hup_signal = signal(SignalKind::hangup())?;
    let mut quit_signal = signal(SignalKind::quit())?;

    let outcome = loop {
        let far = Instant::now() + Duration::from_secs(3600);
        let keys_at = shell.keys_at.unwrap_or(far);
        let repaint_at = shell.repaint_at.unwrap_or(far);
        let keys = tokio::select! {
            event = link.next() => match event {
                LinkEvent::Frame(frame) => {
                    match shell.attachment.handle(frame) {
                        Ok(actions) => {
                            if let Some(outcome) = actions.into_iter().find_map(|a| shell.apply(a)) {
                                break outcome;
                            }
                        }
                        // A lost place in the stream is recovered by a fresh
                        // OPEN, as the protocol says; any other fault would
                        // only repeat.
                        Err(Error::Desync(_)) => shell.reopen = true,
                        Err(err) => break Outcome::Failed(err.to_string()),
                    }
                    if shell.reopen {
                        // Afresh, on a new connection: the attachment's
                        // state is not to be trusted, and a new OPEN
                        // after CLOSE needs the CLOSED first anyway.
                        shell.reopen = false;
                        shell.attachment = Attachment::new();
                        link = Link::Down(reconnect(shell.opts.endpoint.clone()));
                    }
                    continue;
                }
                // A server fault would only repeat; a broken stream is
                // reconnected.
                LinkEvent::Lost(Some(err @ (Error::Server(_) | Error::Protocol(_)))) => {
                    break Outcome::Failed(err.to_string());
                }
                LinkEvent::Lost(_) => {
                    link = Link::Down(reconnect(shell.opts.endpoint.clone()));
                    continue;
                }
                LinkEvent::Refused(err) => break Outcome::Failed(err.to_string()),
                LinkEvent::Connected(conn) => {
                    // A `snapshot` answer repaints; `resume` continues.
                    link = match open(&mut shell, &conn) {
                        Ok(frames) => Link::Up { conn, frames },
                        Err(_) => Link::Down(reconnect(shell.opts.endpoint.clone())),
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
    write_out(&[RESET_MODES, SHOW_CURSOR].concat());
    drop(raw);
    let (message, code) = match outcome {
        Outcome::Detached => ("detached".to_string(), 0),
        Outcome::Exited(Exit::Code { code }) => (
            format!("session exited with code {code}"),
            if code == 0 { 0 } else { 1 },
        ),
        Outcome::Exited(Exit::Signal { signal }) => (format!("session killed by SIG{signal}"), 1),
        Outcome::Restored => (
            "session restored from a previous server; no process".to_string(),
            0,
        ),
        Outcome::Closed(Closed {
            reason: ClosedReason::Removed,
            ..
        }) => ("session removed".to_string(), 0),
        Outcome::Closed(Closed {
            reason: ClosedReason::Detached,
            message,
        }) => (
            match message {
                Some(message) => format!("detached by another client: {message}"),
                None => "detached by another client".to_string(),
            },
            0,
        ),
        Outcome::Closed(Closed { reason, .. }) => (format!("closed: {reason}"), 0),
        Outcome::Failed(message) => (message, 1),
    };
    eprintln!("\r\n[clankermux: {message}]");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(detach_key: u8) -> Shell {
        Shell::new(
            Options {
                endpoint: Endpoint::Unix(Default::default()),
                session: String::new(),
                read_only: false,
                detach_key,
            },
            Size {
                cols: 80,
                rows: 24,
                cell_width: None,
                cell_height: None,
            },
        )
    }

    fn feed(shell: &mut Shell, bytes: &[u8]) -> Option<Vec<u8>> {
        shell.pending.extend_from_slice(bytes);
        match shell.keys(false) {
            Keys::Send(out) => Some(out),
            Keys::Detach => None,
        }
    }

    #[test]
    fn the_prefix_is_recognised_raw_and_kitty_encoded() {
        let mut s = shell(0x1c);
        assert_eq!(feed(&mut s, b"a\x1c"), Some(b"a".to_vec()));
        assert_eq!(
            feed(&mut s, b"\x1c"),
            Some(b"\x1c".to_vec()),
            "twice sends once"
        );
        assert_eq!(
            feed(&mut s, b"\x1cx"),
            Some(b"\x1cx".to_vec()),
            "other keys follow"
        );
        assert_eq!(
            feed(&mut s, b"\x1b[92;5u"),
            Some(vec![]),
            "ctrl+\\ under kitty arms"
        );
        assert_eq!(feed(&mut s, b"\x1b[100u"), None, "kitty d detaches");
        // With release events reported, the prefix's release comes first.
        let mut s = shell(0x1c);
        assert_eq!(feed(&mut s, b"\x1b[92;5u"), Some(vec![]));
        assert_eq!(feed(&mut s, b"\x1b[92;5:3u"), Some(vec![]), "still armed");
        assert_eq!(feed(&mut s, b"\x1b[100u"), None);
        // Not `d`: the prefix, its release and the key all go through.
        let mut s = shell(0x1c);
        assert_eq!(feed(&mut s, b"\x1b[92;5u\x1b[92;5:3u"), Some(vec![]));
        assert_eq!(
            feed(&mut s, b"\x1b[120u"),
            Some(b"\x1b[92;5u\x1b[92;5:3u\x1b[120u".to_vec())
        );
        // Caps lock set, and `d` with an empty modifier field.
        let mut s = shell(0x1c);
        assert_eq!(feed(&mut s, b"\x1b[92;69u"), Some(vec![]));
        assert_eq!(feed(&mut s, b"\x1b[100;;100u"), None);
        let mut s = shell(0x1c);
        assert_eq!(
            feed(&mut s, b"\x1b[97;1:3u"),
            Some(b"\x1b[97;1:3u".to_vec()),
            "other releases pass"
        );
        let mut s = shell(0x1c);
        assert_eq!(
            feed(&mut s, b"\x1b[92;3u"),
            Some(b"\x1b[92;3u".to_vec()),
            "alt+\\ is not the prefix"
        );
        assert_eq!(feed(&mut s, b"\x1cd"), None);
    }

    #[test]
    fn the_clip_window_is_the_bottom_unless_the_cursor_is_above_it() {
        assert_eq!(clip_window(40, 24, 39), 16);
        assert_eq!(
            clip_window(40, 24, 16),
            16,
            "cursor on the window's top row"
        );
        assert_eq!(
            clip_window(40, 24, 15),
            0,
            "just above: the window ends at the cursor"
        );
        assert_eq!(clip_window(40, 24, 30), 16, "mid-window keeps the bottom");
        assert_eq!(clip_window(24, 24, 5), 0);
        assert_eq!(clip_window(60, 24, 40), 36);
        assert_eq!(clip_window(60, 24, 30), 7);
    }
}
