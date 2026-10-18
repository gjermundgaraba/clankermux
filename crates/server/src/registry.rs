//! All sessions and the subscribers to their events.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::{ClosedReason, ErrorCode, Event, Exit, FrameType, Limits, OnExit, Page, Status};
use rustix::process::Signal;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::connection::Writer;
use crate::park;
use crate::records::Records;
use crate::session::{
    Body, Effects, InputQueue, Live, Parked, SNAPSHOT_RETRY, Session, Shared, now,
    snapshot_with_retry,
};
use crate::stream::Stream;
use crate::{frame_json, pty};

pub type Fail = (ErrorCode, String);

/// Running sessions are parked this often.
const PARK_INTERVAL: Duration = Duration::from_secs(300);
/// JSON overhead allowed around a page's sessions within `max_frame`.
const PAGE_MARGIN: usize = 4096;
/// How long a stop waits for the attachments to end, and then for the
/// connections to flush and close.
const STOP_GRACE: Duration = Duration::from_secs(2);

pub struct Config {
    pub limits: Limits,
    /// Default `argv`.
    pub shell: Vec<String>,
    /// Default `cwd`.
    pub home: PathBuf,
    pub version: String,
    /// Holds `lock` and `sessions/`.
    pub state_dir: PathBuf,
}

pub struct Registry {
    pub config: Config,
    pub incarnation: String,
    state: Mutex<State>,
    /// Set by `stop`: nothing touches the state directory after it, and
    /// no session is created.
    stopping: AtomicBool,
    /// Cancelled by `stop` once every attachment has ended: connections
    /// flush and close.
    pub(crate) shutdown: CancellationToken,
    /// The attachment tasks, joined by `stop` before the connections go.
    pub(crate) attachments: TaskTracker,
    /// The connection tasks, joined by `stop` before the lock is released.
    pub(crate) connections: TaskTracker,
    /// The exclusive lock on the state directory, held until `stop`.
    lock: File,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<String, Shared>,
    subscribers: Vec<Writer>,
}

impl Registry {
    /// Locks the state directory (another server holding it is an error),
    /// loads the parked sessions and starts the park timer. Needs a tokio
    /// runtime.
    pub fn new(config: Config) -> io::Result<Arc<Self>> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&config.state_dir)?;
        let lock_path = config.state_dir.join("lock");
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |err| match err {
                rustix::io::Errno::WOULDBLOCK => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("another server holds {}", lock_path.display()),
                ),
                err => io::Error::from(err),
            },
        )?;
        let loaded = park::load(&config.state_dir.join("sessions"));
        let registry = Arc::new(Registry {
            config,
            incarnation: uuid::Uuid::new_v4().to_string(),
            state: Mutex::default(),
            stopping: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
            attachments: TaskTracker::new(),
            connections: TaskTracker::new(),
            lock,
        });
        for loaded in loaded {
            registry.restore(loaded);
        }
        // The timer does not keep the registry alive and ends with `stop`.
        let timer = Arc::downgrade(&registry);
        let shutdown = registry.shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PARK_INTERVAL);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = shutdown.cancelled() => return,
                }
                let Some(registry) = timer.upgrade() else {
                    return;
                };
                for session in registry.sessions() {
                    let running = session.lock().unwrap().status() == Status::Running;
                    if running {
                        registry.park(&session).await;
                    }
                }
            }
        });
        Ok(registry)
    }

    fn sessions(&self) -> Vec<Shared> {
        self.state
            .lock()
            .unwrap()
            .sessions
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    fn sessions_dir(&self) -> PathBuf {
        self.config.state_dir.join("sessions")
    }

    /// A session from its park file: `exited` if it was, `restored` else.
    fn restore(&self, loaded: park::Loaded) {
        let park::Loaded {
            file,
            screen,
            snapshot_path,
        } = loaded;
        let info = file.session;
        let limits = &self.config.limits;
        let session = Session {
            id: info.id.clone(),
            label: info.label,
            argv: info.argv,
            cwd: info.cwd,
            on_exit: info.on_exit,
            created_at: info.created_at,
            exited_at: info.exited_at,
            exit: info.exit,
            wake: Arc::new(Notify::new()),
            text_limit: limits.text as usize,
            records_limit: limits.records as usize,
            attached: Vec::new(),
            records: Records::load(info.program_status.unwrap_or_default()),
            events: Vec::new(),
            removed: false,
            announced: Default::default(),
            park_lock: Default::default(),
            write_chain: None,
            last_generation: 0,
            body: Body::Parked(Parked {
                engine: file.engine,
                cols: info.cols,
                rows: info.rows,
                title: info.title,
                pwd: info.pwd,
                offset: info.offset,
                marker: info.marker,
                screen,
                history_lines: file.history_lines as usize,
                snapshot_path,
            }),
        };
        self.state
            .lock()
            .unwrap()
            .sessions
            .insert(info.id, Arc::new(Mutex::new(session)));
    }

    /// Parks a live session (the timer and the end of a kept session):
    /// nothing once the server is stopping, whose own parks come first.
    pub async fn park(self: &Arc<Self>, session: &Shared) {
        let park_lock = session.lock().unwrap().park_lock.clone();
        let _one_at_a_time = park_lock.lock().await;
        if !self.is_stopping() {
            self.park_locked(session).await;
        }
    }

    /// Writes a generation for a live session under its park lock,
    /// retrying the snapshot while the continuation limit is exceeded. A
    /// failure keeps the previous generation and is logged. A removal or
    /// an `on_exit: remove` end that overlaps the write takes the files
    /// with it.
    async fn park_locked(self: &Arc<Self>, session: &Shared) {
        let id = session.lock().unwrap().id.clone();
        let at = jiff::Timestamp::now();
        let taken = snapshot_with_retry(session, SNAPSHOT_RETRY, |s, live, bytes| {
            s.park_file(live, at)
                .map(|(file, screen)| (bytes, file, screen))
        })
        .await;
        let result = match taken {
            Ok(None) => return,
            Ok(Some(Ok((bytes, file, screen)))) => {
                let generation = {
                    let mut s = session.lock().unwrap();
                    s.last_generation = (at.as_millisecond() as u64).max(s.last_generation + 1);
                    s.last_generation
                };
                park::write(&self.sessions_dir(), &file, generation, &bytes, &screen)
                    .map_err(|e| e.to_string())
            }
            Ok(Some(Err(err))) | Err(err) => Err(err.to_string()),
        };
        if session.lock().unwrap().gone() {
            park::remove(&self.sessions_dir(), &id);
        }
        if let Err(error) = result {
            tracing::warn!(session = id, error, "park failed; previous files kept");
        }
    }

    /// Graceful stop: parks every session that is live and not going
    /// (running, or exited and kept) and deletes the files of those
    /// going, closes every attachment with `stopping` and hangs up each
    /// running session's process group, then closes the connections once
    /// the attachments have queued their `CLOSED`, and releases the state
    /// directory once the connections have flushed (the attachment and
    /// connection waits bounded by `STOP_GRACE`).
    pub async fn stop(self: &Arc<Self>) {
        self.stopping.store(true, Ordering::Release);
        // Taking a park lock joins a park in flight; a later one does
        // nothing, so every session is parked here (a kept session that
        // exited may still be waiting for its final park), all at once so
        // the phase lasts one snapshot retry past any park in flight.
        let mut parks = tokio::task::JoinSet::new();
        for session in self.sessions() {
            let registry = self.clone();
            parks.spawn(async move {
                let park_lock = session.lock().unwrap().park_lock.clone();
                let _one_at_a_time = park_lock.lock().await;
                let (id, gone) = {
                    let s = session.lock().unwrap();
                    (s.id.clone(), s.gone())
                };
                if gone {
                    park::remove(&registry.sessions_dir(), &id);
                } else {
                    registry.park_locked(&session).await;
                }
            });
        }
        while parks.join_next().await.is_some() {}
        for session in self.sessions() {
            let s = session.lock().unwrap();
            s.close_attachments(ClosedReason::Stopping, false);
            if let Ok(live) = s.running() {
                pty::signal(live.pgid, &live.slave_path, Signal::HUP);
            }
        }
        self.attachments.close();
        let _ = tokio::time::timeout(STOP_GRACE, self.attachments.wait()).await;
        self.shutdown.cancel();
        self.connections.close();
        let _ = tokio::time::timeout(STOP_GRACE, self.connections.wait()).await;
        // Nothing of this registry touches the directory from here on, so
        // the next server may take it.
        let _ = rustix::fs::flock(&self.lock, rustix::fs::FlockOperation::Unlock);
    }

    pub fn get(&self, id: &str) -> Result<Shared, Fail> {
        self.state
            .lock()
            .unwrap()
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| (ErrorCode::NoSuchSession, format!("no session {id}")))
    }

    /// Answers a `list` or `subscribe` with a page under the state lock,
    /// so no event raised after the page is queued before it; a
    /// `subscribe` then registers the connection for events.
    pub fn reply_page(&self, writer: &Writer, id: i64, after: Option<&str>, subscribe: bool) {
        let mut state = self.state.lock().unwrap();
        let page = self.page_of(&state, after);
        crate::connection::reply(writer, id, Ok(serde_json::json!(page)));
        if subscribe {
            state.subscribers.push(writer.clone());
        }
    }

    /// Up to `limits.page` sessions after `after`, fewer when the page
    /// would not fit `max_frame` as JSON (a session's text fields escape
    /// to up to six bytes a character); `next` names the last one when
    /// more follow.
    fn page_of(&self, state: &State, after: Option<&str>) -> Page {
        let page = self.config.limits.page as usize;
        let budget = (self.config.limits.max_frame as usize).saturating_sub(PAGE_MARGIN);
        let mut sessions = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for s in state
            .sessions
            .range::<str, _>((
                after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
                std::ops::Bound::Unbounded,
            ))
            .map(|(_, s)| s)
        {
            let session = s.lock().unwrap().to_proto(false);
            bytes += serde_json::to_vec(&session).map_or(0, |v| v.len() + 1);
            if sessions.len() == page || (!sessions.is_empty() && bytes > budget) {
                more = true;
                break;
            }
            sessions.push(session);
        }
        // `next` is the last session sent, per the protocol.
        let next = more
            .then(|| sessions.last().map(|s| s.id.clone()))
            .flatten();
        Page { sessions, next }
    }

    /// Registers a subscriber, answering request `id` with the first page
    /// before any event the subscriber gets: both under the lock, so an
    /// event produced after the page cannot precede it.
    pub fn emit(&self, event: &Event) {
        let bytes = frame_json(FrameType::Event, 0, event);
        self.state
            .lock()
            .unwrap()
            .subscribers
            .retain(|w| w.send(bytes.clone()));
    }

    pub fn emit_all(&self, events: Vec<Event>) {
        for event in &events {
            self.emit(event);
        }
    }

    /// Emits what a session raised under its lock, then `session_changed`
    /// with `changed` when there is one.
    pub fn emit_session(&self, events: Vec<Event>, changed: Option<proto::Session>) {
        self.emit_all(events);
        if let Some(session) = changed {
            self.emit(&Event::SessionChanged { session });
        }
    }

    pub fn create(self: &Arc<Self>, params: proto::CreateParams) -> Result<proto::Session, Fail> {
        if self.is_stopping() {
            return Err((ErrorCode::Internal, "server stopping".into()));
        }
        let limits = &self.config.limits;
        let label = params.label.unwrap_or_default();
        if label.len() > limits.label as usize {
            return Err((ErrorCode::LimitExceeded, "label too long".into()));
        }
        let argv = params.argv.unwrap_or_else(|| self.config.shell.clone());
        let cwd = params
            .cwd
            .unwrap_or_else(|| self.config.home.to_string_lossy().into_owned());
        if cwd.len() > limits.text as usize
            || argv.iter().map(|a| a.len()).sum::<usize>() > limits.text as usize
        {
            return Err((ErrorCode::LimitExceeded, "cwd or argv too long".into()));
        }
        if argv.is_empty()
            || argv.iter().chain([&cwd]).any(|s| s.contains('\0'))
            || params
                .env
                .iter()
                .flatten()
                .any(|(k, v)| k.contains('\0') || v.contains('\0'))
        {
            return Err((
                ErrorCode::InvalidRequest,
                "argv must not be empty, and argv, cwd and env must not hold NUL".into(),
            ));
        }
        let cols = params.cols.unwrap_or(80);
        let rows = params.rows.unwrap_or(24);
        crate::session::check_size(&proto::Size {
            cols,
            rows,
            cell_width: None,
            cell_height: None,
        })?;
        let mut env = BTreeMap::from([("TERM".to_string(), "xterm-ghostty".to_string())]);
        env.extend(params.env.unwrap_or_default());

        // Everything that can fail is tried before the child exists.
        let mut term =
            ghostty_vt::Terminal::with_handler(cols, rows, Effects::default()).map_err(internal)?;
        term.set_xtversion(&format!("clankermux {}", self.config.version));
        term.set_terminfo_name("xterm-ghostty").map_err(internal)?;
        for kind in [
            ghostty_vt::ColorKind::Foreground,
            ghostty_vt::ColorKind::Background,
            ghostty_vt::ColorKind::Cursor,
        ] {
            term.set_color(kind, Some(crate::session::default_color(kind)))
                .map_err(internal)?;
        }
        term.set_scrollback_max_bytes(limits.scrollback as usize)
            .map_err(internal)?;
        term.set_continuation_max_bytes(limits.continuation as usize)
            .map_err(internal)?;
        term.handler_mut().size = ghostty_vt::SizeReport {
            cols,
            rows,
            cell_width: 0,
            cell_height: 0,
        };
        let (pty, mut child) = pty::spawn(&argv, std::path::Path::new(&cwd), &env, cols, rows)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound
                | std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::NotADirectory => (
                    ErrorCode::InvalidRequest,
                    format!("cannot start {argv:?}: {e}"),
                ),
                _ => internal(e),
            })?;
        let pid = child.id().expect("a just spawned child has a pid");
        // SAFETY: the master is an owned, non-blocking fd registered once.
        // A failure drops the child, which tokio reaps when it exits.
        let master = Arc::new(unsafe { AsyncFd::register(pty.master) }.map_err(internal)?);
        let (input, input_rx) = InputQueue::new(limits.input_queue as usize);
        let session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            label,
            argv,
            cwd,
            on_exit: params.on_exit.unwrap_or(OnExit::Remove),
            created_at: now(),
            exited_at: None,
            exit: None,
            wake: Arc::new(Notify::new()),
            text_limit: limits.text as usize,
            records_limit: limits.records as usize,
            attached: Vec::new(),
            records: Records::default(),
            events: Vec::new(),
            removed: false,
            announced: Default::default(),
            park_lock: Default::default(),
            write_chain: None,
            last_generation: 0,
            body: Body::Live(Box::new(Live {
                pid: Some(pid),
                pgid: pid,
                term,
                stream: Stream::new(limits.ring as usize, limits.markers as usize),
                master: master.clone(),
                slave_path: pty.slave_path,
                input,
                pty_eof: false,
                pending: None,
                timer_running: false,
            })),
        };
        let id = session.id.clone();
        let snapshot = session.to_proto(false);
        let shared = Arc::new(Mutex::new(session));
        self.state
            .lock()
            .unwrap()
            .sessions
            .insert(id.clone(), shared.clone());
        self.emit(&Event::SessionCreated {
            session: snapshot.clone(),
        });

        let registry = self.clone();
        let reader_session = shared.clone();
        let reader_master = master.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut guard = match reader_master.readable().await {
                    Ok(guard) => guard,
                    Err(_) => break,
                };
                match guard.try_io(|fd| {
                    rustix::io::read(fd.get_ref(), &mut buf).map_err(std::io::Error::from)
                }) {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        let events = {
                            let mut session = reader_session.lock().unwrap();
                            session.output(&buf[..n]);
                            session.take_events()
                        };
                        registry.emit_all(events);
                    }
                    // EIO is how Linux reports the slave side closing.
                    Ok(Err(_)) => break,
                    Err(_would_block) => continue,
                }
            }
            let ended = {
                let mut session = reader_session.lock().unwrap();
                if let Some(live) = session.live_mut() {
                    live.pty_eof = true;
                }
                session.maybe_end().map(|_| session.to_proto(false))
            };
            registry.ended(&reader_session, ended);
        });

        let registry = self.clone();
        let wait_session = shared.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            let ended = {
                let mut session = wait_session.lock().unwrap();
                if let Some(live) = session.live_mut() {
                    live.pid = None;
                }
                session.exit = Some(match status {
                    Ok(status) => exit_of(status),
                    Err(_) => Exit::Code { code: -1 },
                });
                session.maybe_end().map(|_| session.to_proto(false))
            };
            registry.ended(&wait_session, ended);
        });

        tokio::spawn(async move {
            let mut rx = input_rx;
            while let Some((bytes, permit, done)) = rx.recv().await {
                let mut written = 0;
                while written < bytes.len() {
                    let Ok(mut guard) = master.writable().await else {
                        return;
                    };
                    match guard.try_io(|fd| {
                        rustix::io::write(fd.get_ref(), &bytes[written..])
                            .map_err(std::io::Error::from)
                    }) {
                        Ok(Ok(n)) => written += n,
                        // The waiter sees a dropped `done` as a failed write.
                        Ok(Err(_)) => return,
                        Err(_would_block) => continue,
                    }
                }
                drop(permit);
                let _ = done.send(());
            }
        });

        Ok(snapshot)
    }

    /// Emits `session_exited` when a stream ended, then schedules removal
    /// or parks the session once for `on_exit: keep`. After `stop` the
    /// directory is another server's: nothing is written or deleted.
    fn ended(self: &Arc<Self>, session: &Shared, ended: Option<proto::Session>) {
        let Some(snapshot) = ended else { return };
        let events = session.lock().unwrap().take_events();
        self.emit_all(events);
        let remove = snapshot.on_exit == OnExit::Remove;
        let id = snapshot.id.clone();
        self.emit(&Event::SessionExited { session: snapshot });
        if self.is_stopping() {
            return;
        }
        if remove {
            // Its timer parks say `running`; a stop within the grace must
            // not bring it back.
            park::remove(&self.sessions_dir(), &id);
            self.remove_after_grace(id);
        } else {
            let registry = self.clone();
            let session = session.clone();
            tokio::spawn(async move { registry.park(&session).await });
        }
    }

    fn remove_after_grace(self: &Arc<Self>, id: String) {
        let registry = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(
                registry.config.limits.remove_grace,
            ))
            .await;
            if !registry.is_stopping() {
                registry.remove_now(&id);
            }
        });
    }

    /// An explicit `remove`: finished attachments close at once; when
    /// others are open the session stays until `remove_grace` has passed.
    pub fn remove(self: &Arc<Self>, id: &str) -> Result<(), Fail> {
        if self.is_stopping() {
            return Err((ErrorCode::Internal, "server stopping".into()));
        }
        let session = self.get(id)?;
        let all_finished = {
            let s = session.lock().unwrap();
            if s.status() == Status::Running {
                return Err((ErrorCode::Running, "session is running".into()));
            }
            let all_finished = s.attachments_finished();
            if !all_finished {
                s.close_attachments(ClosedReason::Removed, true);
            }
            all_finished
        };
        if all_finished {
            self.remove_now(id);
        } else {
            // The files stay for the grace (a parked session serves its
            // snapshot from them); a stop within it deletes them.
            session.lock().unwrap().removed = true;
            self.remove_after_grace(id.to_string());
        }
        Ok(())
    }

    /// Drops the session and its files, closing every attachment.
    fn remove_now(&self, id: &str) {
        let Some(session) = self.state.lock().unwrap().sessions.remove(id) else {
            return;
        };
        {
            let mut s = session.lock().unwrap();
            s.removed = true;
            s.close_attachments(ClosedReason::Removed, false);
        }
        park::remove(&self.sessions_dir(), id);
        self.emit(&Event::SessionRemoved { id: id.to_string() });
    }
}

fn exit_of(status: std::process::ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(signal) => Exit::Signal {
            signal: pty::signal_name(signal),
        },
        None => Exit::Code {
            code: status.code().unwrap_or(-1),
        },
    }
}

pub fn internal(err: impl std::fmt::Display) -> Fail {
    (ErrorCode::Internal, err.to_string())
}
