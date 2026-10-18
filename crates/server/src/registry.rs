//! All sessions and the subscribers to their events.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::{ClosedReason, ErrorCode, Event, Exit, FrameType, Limits, OnExit, Page, Status};
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;

use crate::connection::Writer;
use crate::park;
use crate::records::Records;
use crate::session::{
    Body, Effects, InputQueue, Live, Parked, Session, Shared, now, snapshot_with_retry,
};
use crate::stream::Stream;
use crate::{frame_json, pty};

pub type Fail = (ErrorCode, String);

/// Running sessions are parked this often.
const PARK_INTERVAL: Duration = Duration::from_secs(300);
/// JSON overhead allowed around a page's sessions within `max_frame`.
const PAGE_MARGIN: usize = 4096;

/// How long a park retries while the continuation limit is exceeded.
const PARK_RETRY: Duration = Duration::from_secs(5);

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
    stopped: Arc<Notify>,
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
            stopped: Arc::new(Notify::new()),
            lock,
        });
        for loaded in loaded {
            registry.restore(loaded);
        }
        // The timer does not keep the registry alive and ends with `stop`.
        let timer = Arc::downgrade(&registry);
        let stopped = registry.stopped.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PARK_INTERVAL);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = stopped.notified() => return,
                }
                let Some(registry) = timer.upgrade() else {
                    return;
                };
                registry.park_running().await;
            }
        });
        Ok(registry)
    }

    fn is_stopping(&self) -> bool {
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
        let exited = info.status == Status::Exited;
        let session = Session {
            id: info.id.clone(),
            label: info.label,
            argv: info.argv,
            cwd: info.cwd,
            on_exit: info.on_exit,
            created_at: info.created_at,
            // An exited session always has its time; the park's stands in
            // for a file that lacks it.
            exited_at: info
                .exited_at
                .or_else(|| exited.then(|| file.parked_at.clone())),
            exit: info.exit.filter(|_| exited),
            wake: Arc::new(Notify::new()),
            text_limit: limits.text as usize,
            records_limit: limits.records as usize,
            attached: Vec::new(),
            records: Records::load(info.program_status.unwrap_or_default()),
            events: Vec::new(),
            removed: false,
            park_lock: Default::default(),
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

    /// Parks every running session.
    async fn park_running(self: &Arc<Self>) {
        let running: Vec<Shared> = self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter(|s| s.lock().unwrap().status() == Status::Running)
            .cloned()
            .collect();
        for session in running {
            self.park(&session).await;
        }
    }

    /// Writes a generation for a live session, retrying the snapshot while
    /// the continuation limit is exceeded. A failure keeps the previous
    /// generation and is logged. Parks of one session run one at a time,
    /// and a removal that overlaps one takes its files with it.
    pub async fn park(self: &Arc<Self>, session: &Shared) {
        let (id, park_lock) = {
            let s = session.lock().unwrap();
            (s.id.clone(), s.park_lock.clone())
        };
        let _one_at_a_time = park_lock.lock().await;
        let at = jiff::Timestamp::now();
        let taken = snapshot_with_retry(session, PARK_RETRY, |s, live, bytes| {
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
        if session.lock().unwrap().removed {
            park::remove(&self.sessions_dir(), &id);
        }
        if let Err(error) = result {
            tracing::warn!(session = id, error, "park failed; previous files kept");
        }
    }

    /// Graceful stop: parks every running session, closes every
    /// attachment with `stopping`, and hangs up each running session's
    /// process group.
    pub async fn stop(self: &Arc<Self>) {
        self.stopping.store(true, Ordering::Release);
        self.stopped.notify_waiters();
        self.park_running().await;
        let sessions: Vec<Shared> = self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .cloned()
            .collect();
        for session in sessions {
            let s = session.lock().unwrap();
            s.close_attachments(ClosedReason::Stopping, false);
            if let Ok(live) = s.running() {
                pty::signal(
                    live.pgid,
                    &live.slave_path,
                    pty::signal_by_name("HUP").unwrap(),
                );
            }
        }
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

    pub fn page(&self, after: Option<&str>) -> Page {
        self.page_of(&self.state.lock().unwrap(), after)
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
        let mut next = None;
        for (id, s) in state.sessions.range::<str, _>((
            after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
            std::ops::Bound::Unbounded,
        )) {
            let session = s.lock().unwrap().to_proto(false);
            bytes += serde_json::to_vec(&session).map_or(0, |v| v.len() + 1);
            if sessions.len() == page || (!sessions.is_empty() && bytes > budget) {
                next = Some(id.clone());
                break;
            }
            sessions.push(session);
        }
        // `next` is the last session sent, per the protocol.
        let next = next.and_then(|_| sessions.last().map(|s| s.id.clone()));
        Page { sessions, next }
    }

    /// Registers a subscriber and returns the first page as of that moment.
    pub fn subscribe(&self, writer: Writer) -> Page {
        let mut state = self.state.lock().unwrap();
        state.subscribers.push(writer);
        self.page_of(&state, None)
    }

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
        let mut term = ghostty_vt::Terminal::new(cols, rows)
            .map_err(internal)?
            .with_handler(Effects::default());
        term.set_xtversion(&format!("clankermux {}", self.config.version));
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
            park_lock: Default::default(),
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
                profile: proto::Profile::default(),
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
