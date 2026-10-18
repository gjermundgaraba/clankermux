//! The terminal side of `attach`: raw mode, the outer size, passthrough
//! or repaint, keys, the detach prefix, and reconnecting with resume.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use client::connection::hello;
use client::{Action, Attachment, Connection, Error, Frame};
use ghostty_vt::formatter::{self, Format, Region};
use proto::{ClosedReason, Exit, FrameType, Open, Size, Viewport};
use rustix::termios::{self, OptionalActions, Termios};
use tokio::io::AsyncReadExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

pub struct Options {
    pub socket: PathBuf,
    pub session: String,
    pub read_only: bool,
    /// The prefix byte; followed by `d` it detaches, twice it is sent.
    pub detach_key: u8,
}

/// `C-x` or `^x` for a control key, else one byte.
pub fn parse_key(spec: &str) -> Option<u8> {
    let bytes = spec.as_bytes();
    match bytes {
        [b'C', b'-', c] | [b'^', c] if c.is_ascii_graphic() => Some(c.to_ascii_uppercase() ^ 0x40),
        [c] => Some(*c),
        _ => None,
    }
}

/// How long to wait for the rest of an escape sequence from stdin.
const KEY_TIMEOUT: Duration = Duration::from_millis(25);
/// Coalescing window for repaints while the sizes differ.
const REPAINT: Duration = Duration::from_millis(16);
const CHANNEL: u32 = 1;

/// Resets what passthrough may have left set on the outer terminal.
const RESET: &[u8] = b"\x1b[0m\x1b[?25h\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[>4;0m\x1b[<u";

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
    attachment: Attachment,
    outer: Size,
    /// Output goes straight to the outer terminal while its size equals
    /// the session's; otherwise the screen is repainted from the engine.
    passthrough: bool,
    repaint_due: bool,
    /// Stdin bytes that do not yet form whole key encodings.
    pending: Vec<u8>,
    /// The detach prefix was pressed.
    armed: bool,
}

/// Why the attach ended.
enum Outcome {
    Detached,
    Exited(Exit),
    Closed(ClosedReason),
    Failed(String),
}

impl Shell {
    fn sizes_match(&self) -> bool {
        (self.outer.cols, self.outer.rows) == (self.attachment.cols, self.attachment.rows)
    }

    fn set_mode(&mut self) {
        self.passthrough = self.sizes_match();
        self.attachment.set_passthrough(self.passthrough);
    }

    fn paint(&mut self) {
        let Some(term) = self.attachment.term() else {
            return;
        };
        let region = if self.passthrough {
            Region::All
        } else {
            Region::Clip {
                cols: self.outer.cols,
                rows: self.outer.rows,
            }
        };
        let painted = formatter::format(
            term,
            formatter::Options {
                format: Format::Vt,
                cursor: true,
                style: true,
                hyperlink: true,
                modes: true,
                scrolling_region: self.passthrough,
                keyboard: true,
                region,
                ..Default::default()
            },
        );
        let mut out = b"\x1b[0m\x1b[?25l\x1b[H\x1b[2J".to_vec();
        if let Ok(painted) = painted {
            out.extend_from_slice(&painted);
        }
        out.extend_from_slice(b"\x1b[?25h");
        write_out(&out);
        self.repaint_due = false;
    }

    fn apply(&mut self, action: Action) -> Option<Outcome> {
        match action {
            Action::Screen => {
                self.set_mode();
                self.paint();
            }
            Action::Output(bytes) => {
                if self.passthrough {
                    write_out(&bytes);
                } else {
                    self.repaint_due = true;
                }
            }
            Action::Resized { .. } => {
                self.set_mode();
                self.paint();
            }
            Action::Gap(_) => {}
            Action::Exited(exit) => return Some(Outcome::Exited(exit)),
            Action::Closed(reason) => return Some(Outcome::Closed(reason)),
            Action::Error(error) => {
                if self.attachment.ended() {
                    return Some(Outcome::Failed(format!(
                        "{:?}: {}",
                        error.code, error.message
                    )));
                }
                write_out(b"\x07");
            }
        }
        None
    }

    /// Splits stdin bytes into whole keys, handles the detach prefix and
    /// drops the outer terminal's answers to forwarded queries. Returns
    /// the bytes to send, or `None` on detach.
    fn keys(&mut self, flush: bool) -> Option<Vec<u8>> {
        let complete = if flush {
            self.pending.len()
        } else {
            client::vt::complete_prefix(&self.pending)
        };
        let bytes: Vec<u8> = self.pending.drain(..complete).collect();
        let mut out = Vec::with_capacity(bytes.len());
        for b in bytes {
            if self.armed {
                self.armed = false;
                match b {
                    b'd' => return None,
                    k if k == self.opts.detach_key => out.push(k),
                    other => {
                        out.push(self.opts.detach_key);
                        out.push(other);
                    }
                }
            } else if b == self.opts.detach_key {
                self.armed = true;
            } else {
                out.push(b);
            }
        }
        match self.attachment.term() {
            Some(term) => Some(term.handler_mut().filter(&out)),
            None => Some(out),
        }
    }
}

pub async fn run(opts: Options) -> Result<ExitCode, Error> {
    let socket = opts.socket.clone();
    let connect =
        || Connection::connect_unix(&socket, hello("clankermux", Some(ghostty_vt::ENGINE)));
    let mut conn = connect().await?;
    let raw = Raw::enter().map_err(Error::Io)?;
    let outer = outer_size();
    let mut shell = Shell {
        attachment: Attachment::new(
            &conn.welcome.incarnation,
            conn.welcome.limits.continuation as usize,
        ),
        opts,
        outer,
        passthrough: false,
        repaint_due: false,
        pending: Vec::new(),
        armed: false,
    };
    let open =
        |shell: &Shell, conn: &Connection| -> Result<mpsc::UnboundedReceiver<Frame>, Error> {
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
        };
    let mut frames = open(&shell, &conn)?;
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 4096];
    let mut winch = signal(SignalKind::window_change()).map_err(Error::Io)?;
    let mut term_signal = signal(SignalKind::terminate()).map_err(Error::Io)?;
    let mut backoff = Duration::from_millis(250);

    let outcome = loop {
        let key_wait = tokio::time::sleep(KEY_TIMEOUT);
        let repaint_wait = tokio::time::sleep(REPAINT);
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(frame) => {
                    let actions = match shell.attachment.handle(frame) {
                        Ok(actions) => actions,
                        Err(err) => break Outcome::Failed(err.to_string()),
                    };
                    if let Some(outcome) = actions.into_iter().find_map(|a| shell.apply(a)) {
                        break outcome;
                    }
                }
                None => {
                    // Reconnect with resume; a `snapshot` answer repaints.
                    conn.release(CHANNEL);
                    loop {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                        match connect().await {
                            Ok(c) => {
                                conn = c;
                                break;
                            }
                            Err(_) => continue,
                        }
                    }
                    backoff = Duration::from_millis(250);
                    frames = open(&shell, &conn)?;
                }
            },
            read = stdin.read(&mut buf) => match read {
                Ok(0) | Err(_) => break Outcome::Detached,
                Ok(n) => {
                    shell.pending.extend_from_slice(&buf[..n]);
                    match shell.keys(false) {
                        None => break Outcome::Detached,
                        Some(bytes) if !bytes.is_empty() && !shell.opts.read_only => {
                            conn.send(FrameType::Input, CHANNEL, &bytes)?;
                        }
                        Some(_) => {}
                    }
                }
            },
            _ = key_wait, if !shell.pending.is_empty() => {
                match shell.keys(true) {
                    None => break Outcome::Detached,
                    Some(bytes) if !bytes.is_empty() && !shell.opts.read_only => {
                        conn.send(FrameType::Input, CHANNEL, &bytes)?;
                    }
                    Some(_) => {}
                }
            },
            _ = repaint_wait, if shell.repaint_due => shell.paint(),
            _ = winch.recv() => {
                shell.outer = outer_size();
                shell.set_mode();
                if !shell.opts.read_only {
                    conn.send_json(FrameType::Viewport, CHANNEL, &Viewport { size: shell.outer, profile: None })?;
                }
                if !shell.passthrough {
                    shell.paint();
                }
            },
            _ = term_signal.recv() => break Outcome::Detached,
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
