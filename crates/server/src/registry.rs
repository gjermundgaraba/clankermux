//! All sessions and the subscribers to their events.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use proto::{
    ClosedReason, ErrorCode, Event, Exit, FrameType, Limits, OnExit, Page, Status, StatusReason,
    SubscribeParams, Subscribed, encode_json_frame,
};
use rustix::process::Signal;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::commands::Commands;
use crate::connection::{Writer, reply};
use crate::park;
use crate::pty;
use crate::records::Records;
use crate::session::{
    Body, Effects, InputQueue, Live, Parked, SNAPSHOT_RETRY, Session, Shared, blocking, check_size,
    default_color, now, snapshot_with_retry,
};
use crate::stream::Stream;

pub type Fail = (ErrorCode, String);

/// The retained events' bytes, in `max_frame`s: with a first page, a
/// replay stays under a connection's ceiling (`connection::Writer`).
const RETAINED_FRAMES: u64 = 2;
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
    /// The Unix socket, absolute: every child gets it as
    /// `CLANKERMUX_SOCKET`.
    pub socket: PathBuf,
    pub park: ParkTiming,
}

/// When running sessions are parked, besides the last detach and a stop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParkTiming {
    /// Every running session, this often.
    pub interval: Duration,
    /// A session that changed, once it has been quiet this long...
    pub quiet: Duration,
    /// ...but no sooner than this long after its last park began...
    pub gap: Duration,
    /// ...and at the latest this long after its first change since its
    /// last park, or `gap` after that park when later.
    pub busy: Duration,
}

impl Default for ParkTiming {
    fn default() -> Self {
        ParkTiming {
            interval: Duration::from_secs(300),
            quiet: Duration::from_secs(2),
            gap: Duration::from_secs(5),
            busy: Duration::from_secs(30),
        }
    }
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
    /// The `seq` of the latest event.
    seq: u64,
    /// The latest events as sent, oldest first, for `subscribe {after}`.
    retained: VecDeque<(u64, Vec<u8>)>,
    retained_bytes: usize,
}

impl State {
    /// `oldest` and `latest` of the `subscribe` reply.
    fn cursor(&self) -> (u64, u64) {
        let oldest = self.retained.front().map_or(self.seq + 1, |(seq, _)| *seq);
        (oldest, self.seq)
    }
}

/// `EVENT`: the event and its `seq`, as `proto::Numbered` but borrowing
/// the event, which is encoded once for every subscriber.
#[derive(serde::Serialize)]
struct Numbered<'a> {
    seq: u64,
    #[serde(flatten)]
    event: &'a Event,
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
        for loaded in park::load(&registry.sessions_dir()) {
            registry.restore(loaded);
        }
        // The timer does not keep the registry alive and ends with `stop`.
        // Its first tick is one interval after the start; a new session
        // parks sooner, as a change (`park_when_due`).
        let timer = Arc::downgrade(&registry);
        let shutdown = registry.shutdown.clone();
        let period = registry.config.park.interval;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
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
        let (records, dropped) = Records::load(info.program_status.unwrap_or_default());
        let mut session = Session {
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
            commands_limit: limits.commands as usize,
            attached: Vec::new(),
            size_policy: info.size.policy,
            size_owner: None,
            records,
            commands: Commands::load(file.commands),
            events: Vec::new(),
            removed: false,
            announced: Default::default(),
            last_bell: None,
            park_lock: Default::default(),
            unparked: None,
            parked_at: None,
            park_due: Arc::new(Notify::new()),
            send_chain: None,
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
        // What the clients of this incarnation are shown first.
        session.announced = crate::session::announced(&session.to_proto(false));
        self.state
            .lock()
            .unwrap()
            .sessions
            .insert(info.id.clone(), Arc::new(Mutex::new(session)));
        if !dropped.is_empty() {
            self.emit(&Event::ProgramStatusRemoved {
                id: info.id,
                record_ids: dropped,
                reason: StatusReason::Restore,
            });
        }
    }

    /// Parks a live session (the timer, a change, the last detach and
    /// the end of a kept session): nothing once the server is stopping,
    /// whose own parks come first.
    pub async fn park(self: &Arc<Self>, session: &Shared) {
        self.park_if(session, false).await;
    }

    /// `park`, or with `changed` only when the session changed since
    /// the last park began: a park it waited behind may have taken all.
    async fn park_if(self: &Arc<Self>, session: &Shared, changed: bool) {
        let park_lock = session.lock().unwrap().park_lock.clone();
        let _one_at_a_time = park_lock.lock().await;
        if !self.is_stopping() && (!changed || session.lock().unwrap().unparked.is_some()) {
            self.park_locked(session).await;
        }
    }

    /// Writes a generation for a live session under its park lock,
    /// retrying the snapshot while the continuation limit is exceeded. A
    /// failure keeps the previous generation and is logged; the next
    /// change, the last detach or the timer parks again. A removal or
    /// an `on_exit: remove` end that overlaps the write takes the files
    /// with it.
    async fn park_locked(self: &Arc<Self>, session: &Shared) {
        // A change from here on is not certain to be in this park, so it
        // counts towards the next.
        let id = {
            let mut s = session.lock().unwrap();
            s.unparked = None;
            s.parked_at = Some(Instant::now());
            s.id.clone()
        };
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
                // Four fsyncs and a rename: off the async workers.
                blocking(|| park::write(&self.sessions_dir(), &file, generation, &bytes, &screen))
                    .map_err(|e| e.to_string())
            }
            Ok(Some(Err(err))) | Err(err) => Err(err.to_string()),
        };
        // A removal deletes the files on purpose; a write it broke is no
        // failure.
        if session.lock().unwrap().gone() {
            park::remove(&self.sessions_dir(), &id);
        } else if let Err(error) = result {
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

    /// Answers a `list` with a page under the state lock, so no event
    /// raised after the page is queued before it.
    pub fn reply_page(&self, writer: &Writer, id: i64, after: Option<&str>) {
        let state = self.state.lock().unwrap();
        let page = self.page_of(&state, after);
        reply(writer, id, Ok(serde_json::json!(page)));
    }

    /// Answers a `subscribe` under the state lock and, on the
    /// connection's first (`first`), registers it for events: without
    /// `after` the reply holds the first page, with it the retained events
    /// after `after` follow the reply, unless there is a gap. Either way
    /// every later event follows them. Returns whether it registered.
    pub fn subscribe(
        &self,
        writer: &Writer,
        id: i64,
        params: SubscribeParams,
        first: bool,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        let (oldest, latest) = state.cursor();
        let (result, replay) = match (params.after, params.incarnation) {
            (None, None) => {
                let page = self.page_of(&state, None);
                let subscribed = Subscribed {
                    sessions: Some(page.sessions),
                    next: page.next,
                    oldest,
                    latest,
                    gap: None,
                };
                (Ok(subscribed), None)
            }
            (Some(after), Some(incarnation)) => {
                let same = incarnation == self.incarnation;
                if !first {
                    (
                        Err((ErrorCode::InvalidRequest, "already subscribed".into())),
                        None,
                    )
                } else if same && after > latest {
                    let message = format!("after {after} is above the latest event, {latest}");
                    (Err((ErrorCode::InvalidRequest, message)), None)
                } else {
                    let gap = !same || after + 1 < oldest;
                    let subscribed = Subscribed {
                        sessions: None,
                        next: None,
                        oldest,
                        latest,
                        gap: Some(gap),
                    };
                    (Ok(subscribed), (!gap).then_some(after))
                }
            }
            _ => {
                let message = "after and incarnation go together".into();
                (Err((ErrorCode::InvalidRequest, message)), None)
            }
        };
        let ok = result.is_ok();
        reply(writer, id, result.map(|r| serde_json::json!(r)));
        if !ok || !first {
            return false;
        }
        if let Some(after) = replay {
            for (_, bytes) in state.retained.iter().filter(|(seq, _)| *seq > after) {
                if !writer.send(bytes.clone()) {
                    return false;
                }
            }
        }
        state.subscribers.push(writer.clone());
        true
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

    /// Numbers `event`, retains it, and queues it for every subscriber
    /// under the state lock, so it follows any page replied before it.
    /// The retained events are at most `limits.events`, and at most
    /// `RETAINED_FRAMES` frames' worth of bytes, so a replay stays well
    /// within what a connection may queue.
    pub fn emit(&self, event: &Event) {
        let mut state = self.state.lock().unwrap();
        state.seq += 1;
        let seq = state.seq;
        let bytes = encode_json_frame(FrameType::Event, 0, &Numbered { seq, event });
        state.subscribers.retain(|w| w.send(bytes.clone()));
        state.retained_bytes += bytes.len();
        state.retained.push_back((seq, bytes));
        let limits = &self.config.limits;
        while state.retained.len() as u64 > limits.events
            || state.retained_bytes as u64 > RETAINED_FRAMES * limits.max_frame
        {
            let Some((_, dropped)) = state.retained.pop_front() else {
                break;
            };
            state.retained_bytes -= dropped.len();
        }
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
        check_size(&proto::Size {
            cols,
            rows,
            cell_width: None,
            cell_height: None,
        })?;
        // `TERM` is a default; the session's own id and socket are not.
        let id = uuid::Uuid::new_v4().to_string();
        let mut env = BTreeMap::from([("TERM".to_string(), "xterm-ghostty".to_string())]);
        env.extend(params.env.unwrap_or_default());
        env.insert("CLANKERMUX_SESSION".into(), id.clone());
        env.insert(
            "CLANKERMUX_SOCKET".into(),
            self.config.socket.to_string_lossy().into_owned(),
        );

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
            term.set_color(kind, Some(default_color(kind)))
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
                io::ErrorKind::NotFound
                | io::ErrorKind::PermissionDenied
                | io::ErrorKind::NotADirectory => (
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
        let created = Instant::now();
        let session = Session {
            id,
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
            commands_limit: limits.commands as usize,
            attached: Vec::new(),
            size_policy: proto::SizePolicy::Latest,
            size_owner: None,
            records: Records::default(),
            commands: Commands::default(),
            events: Vec::new(),
            removed: false,
            announced: Default::default(),
            last_bell: None,
            park_lock: Default::default(),
            // Created is changed: parked within `quiet` even if the
            // program never writes.
            unparked: Some((created, created)),
            parked_at: None,
            park_due: Arc::new(Notify::new()),
            send_chain: None,
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
                prompts: Default::default(),
                last_output: created,
            })),
        };
        let id = session.id.clone();
        let snapshot = session.to_proto(false);
        let due = session.park_due.clone();
        let shared = Arc::new(Mutex::new(session));
        self.state
            .lock()
            .unwrap()
            .sessions
            .insert(id.clone(), shared.clone());
        self.emit(&Event::SessionCreated {
            session: snapshot.clone(),
        });
        tokio::spawn(park_when_due(
            Arc::downgrade(self),
            Arc::downgrade(&shared),
            due,
            self.shutdown.clone(),
        ));

        let registry = self.clone();
        let reader_session = shared.clone();
        let reader_master = master.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let Ok(mut guard) = reader_master.readable().await else {
                    break;
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

    /// `ack` changed a session's records: they reach its files with its
    /// next park when it runs, with a park now when it ended and is
    /// kept, and with a rewrite of `session.json` alone for a session
    /// loaded from a park file.
    pub fn records_changed(self: &Arc<Self>, session: &Shared) {
        let mut s = session.lock().unwrap();
        if s.gone() || self.is_stopping() {
            return;
        }
        if s.status() == Status::Running {
            s.touch();
            return;
        }
        let parked = s.parked().is_some();
        drop(s);
        let registry = self.clone();
        let session = session.clone();
        tokio::spawn(async move {
            if parked {
                registry.rewrite_records(&session).await;
            } else {
                registry.park(&session).await;
            }
        });
    }

    /// Writes a parked session's records into its `session.json` under
    /// its park lock, outside the session lock: the fsyncs hold up no
    /// one. A removal that overlaps the write takes the files with it.
    async fn rewrite_records(self: &Arc<Self>, session: &Shared) {
        let park_lock = session.lock().unwrap().park_lock.clone();
        let _one_at_a_time = park_lock.lock().await;
        let (id, path, records) = {
            let s = session.lock().unwrap();
            let Some(parked) = s.parked() else { return };
            if s.gone() || self.is_stopping() {
                return;
            }
            (s.id.clone(), parked.snapshot_path.clone(), s.records.all())
        };
        let result = blocking(|| park::rewrite_records(&path, records));
        if session.lock().unwrap().gone() {
            park::remove(&self.sessions_dir(), &id);
        } else if let Err(err) = result {
            tracing::warn!(session = id, error = %err, "records not saved");
        }
    }

    fn remove_after_grace(self: &Arc<Self>, id: String) {
        let registry = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(registry.config.limits.remove_grace)).await;
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
            let mut s = session.lock().unwrap();
            if s.status() == Status::Running {
                return Err((ErrorCode::Running, "session is running".into()));
            }
            let all_finished = s.attachments_finished();
            if !all_finished {
                // The files stay for the grace (a parked session serves
                // its snapshot from them); a stop within it deletes them.
                s.removed = true;
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
            // Its waits end.
            s.wake.notify_waiters();
        }
        park::remove(&self.sessions_dir(), id);
        self.emit(&Event::SessionRemoved { id: id.to_string() });
    }
}

/// A running session's activity park: once its changes have been quiet
/// for `park.quiet`, or `park.busy` after the first of them, whichever
/// comes first, and not within `park.gap` of the last park, so that
/// output in bursts just over `quiet` apart parks once per `gap`. Woken by `due` at the first change after a park; ends
/// with the session's stream, the session or the server.
async fn park_when_due(
    registry: Weak<Registry>,
    session: Weak<Mutex<Session>>,
    due: Arc<Notify>,
    shutdown: CancellationToken,
) {
    loop {
        loop {
            let (Some(registry), Some(shared)) = (registry.upgrade(), session.upgrade()) else {
                return;
            };
            let deadline = {
                let s = shared.lock().unwrap();
                if s.status() != Status::Running {
                    return;
                }
                let timing = registry.config.park;
                match s.unparked {
                    Some((first, last)) => {
                        let due = (last + timing.quiet).min(first + timing.busy);
                        s.parked_at.map_or(due, |at| due.max(at + timing.gap))
                    }
                    // Parked meanwhile, by the timer or a detach.
                    None => break,
                }
            };
            if Instant::now() >= deadline {
                registry.park_if(&shared, true).await;
                break;
            }
            drop((registry, shared));
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = shutdown.cancelled() => return,
            }
        }
        // The session's end notifies too, so that this task ends with it.
        tokio::select! {
            _ = due.notified() => {}
            _ = shutdown.cancelled() => return,
        }
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
