use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use proto::{
    ClosedReason, ErrorCode, Event, Exit, FrameType, Limits, Numbered, OnExit, Page, Status,
    StatusReason, SubscribeParams, Subscribed, encode_json_frame,
};
use rustix::process::Signal;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::commands::Commands;
use crate::connection::{Writer, reply};
use crate::park;
use crate::pty;
use crate::records::{self, Records};
use crate::session::{
    Body, Effects, InputQueue, InputUnit, Live, Parked, SNAPSHOT_RETRY, Session, Shared, Unparked,
    blocking, check_size, default_color, now, snapshot_with_retry,
};
use crate::stream::Stream;

pub type Fail = (ErrorCode, String);

const RETAINED_FRAMES: u64 = 2;
const PAGE_MARGIN: usize = 4096;
const STOP_GRACE: Duration = Duration::from_secs(2);

pub struct Config {
    pub limits: Limits,
    pub shell: Vec<String>,
    pub home: PathBuf,
    pub version: String,
    pub state_dir: PathBuf,
    pub socket: AbsolutePath,
    pub park: ParkTiming,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsolutePath(PathBuf);

impl AbsolutePath {
    pub fn new(path: &Path) -> io::Result<Self> {
        std::path::absolute(path).map(AbsolutePath)
    }
}

impl std::ops::Deref for AbsolutePath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParkTiming {
    pub interval: Duration,
    pub quiet: Duration,
    pub gap: Duration,
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

impl ParkTiming {
    pub fn next_due(&self, changes: Unparked, parked_at: Option<Instant>) -> Instant {
        let due = (changes.latest + self.quiet).min(changes.first + self.busy);
        parked_at.map_or(due, |at| due.max(at + self.gap))
    }
}

pub struct Registry {
    pub config: Config,
    pub incarnation: String,
    state: Mutex<State>,
    stopping: AtomicBool,
    pub(crate) shutdown: CancellationToken,
    pub(crate) attachments: TaskTracker,
    pub(crate) connections: TaskTracker,
    lock: File,
    catalog: OnceLock<proto::catalog::Catalog>,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<String, Shared>,
    subscribers: Vec<Writer>,
    seq: u64,
    retained: VecDeque<(u64, Vec<u8>)>,
    retained_bytes: usize,
}

impl State {
    fn cursor(&self) -> (u64, u64) {
        let oldest = self.retained.front().map_or(self.seq + 1, |(seq, _)| *seq);
        (oldest, self.seq)
    }
}

impl Registry {
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
            catalog: OnceLock::new(),
        });
        for loaded in park::load(&registry.sessions_dir()) {
            registry.restore(loaded);
        }
        let timer = Arc::downgrade(&registry);
        let shutdown = registry.shutdown.clone();
        let period = registry.config.park.interval;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    () = shutdown.cancelled() => return,
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

    fn restore(&self, loaded: park::Loaded) {
        let park::Loaded {
            file,
            screen,
            snapshot_path,
        } = loaded;
        let info = file.session;
        let records::Loaded { records, dropped } =
            Records::load(info.program_status.unwrap_or_default());
        let body = Body::Parked(Parked {
            engine: file.engine,
            cols: info.cols,
            rows: info.rows,
            title: info.title,
            pwd: info.pwd,
            offset: info.offset,
            marker: info.marker,
            screen,
            history_lines: file.history_lines,
            snapshot_path,
        });
        let mut session = Session {
            id: info.id.clone(),
            label: info.label,
            argv: info.argv,
            cwd: info.cwd,
            on_exit: info.on_exit,
            created_at: info.created_at,
            exited_at: info.exited_at,
            exit: info.exit,
            size_policy: info.size.policy,
            records,
            commands: Commands::load(file.commands),
            ..Session::blank(&self.config.limits, body)
        };
        session.announced = crate::session::announced(&session.to_proto(false));
        self.state
            .lock()
            .unwrap()
            .sessions
            .insert(info.id.clone(), Arc::new(Mutex::new(session)));
        if !dropped.is_empty() {
            self.emit(Event::ProgramStatusRemoved {
                id: info.id,
                record_ids: dropped,
                reason: StatusReason::Restore,
            });
        }
    }

    pub async fn park(self: &Arc<Self>, session: &Shared) {
        self.park_if(session, false).await;
    }

    async fn park_if(self: &Arc<Self>, session: &Shared, changed: bool) {
        let park_lock = session.lock().unwrap().park_lock.clone();
        let _one_at_a_time = park_lock.lock().await;
        if !self.is_stopping() && (!changed || session.lock().unwrap().unparked.is_some()) {
            self.park_locked(session).await;
        }
    }

    async fn park_locked(self: &Arc<Self>, session: &Shared) {
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
                    s.last_generation = u64::try_from(at.as_millisecond())
                        .unwrap_or(0)
                        .max(s.last_generation + 1);
                    s.last_generation
                };
                blocking(|| park::write(&self.sessions_dir(), &file, generation, &bytes, &screen))
                    .map_err(|e| e.to_string())
            }
            Ok(Some(Err(err))) | Err(err) => Err(err.to_string()),
        };
        if session.lock().unwrap().gone() {
            park::remove(&self.sessions_dir(), &id);
        } else if let Err(error) = result {
            tracing::warn!(session = id, error, "park failed; previous files kept");
        }
    }

    pub async fn stop(self: &Arc<Self>) {
        self.stopping.store(true, Ordering::Release);
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
            s.close_attachments(ClosedReason::Stopping);
            if let Ok(live) = s.running() {
                pty::signal(live.pgid, &live.slave_path, Signal::HUP);
            }
        }
        self.attachments.close();
        let _ = tokio::time::timeout(STOP_GRACE, self.attachments.wait()).await;
        self.shutdown.cancel();
        self.connections.close();
        let _ = tokio::time::timeout(STOP_GRACE, self.connections.wait()).await;
        let _ = rustix::fs::flock(&self.lock, rustix::fs::FlockOperation::Unlock);
    }

    pub fn catalog(&self) -> &proto::catalog::Catalog {
        self.catalog
            .get_or_init(|| proto::catalog::catalog(Some(&self.config.version)))
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

    pub fn reply_page(&self, writer: &Writer, id: i64, after: Option<&str>) {
        let state = self.state.lock().unwrap();
        let page = self.page_of(&state, after);
        reply(writer, id, Ok(serde_json::json!(page)));
    }

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

    fn page_of(&self, state: &State, after: Option<&str>) -> Page {
        let page = proto::cap(self.config.limits.page);
        let budget = proto::cap(self.config.limits.max_frame).saturating_sub(PAGE_MARGIN);
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
        let next = more
            .then(|| sessions.last().map(|s| s.id.clone()))
            .flatten();
        Page { sessions, next }
    }

    pub fn emit(&self, event: Event) {
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
        for event in events {
            self.emit(event);
        }
    }

    pub fn emit_session(&self, events: Vec<Event>, changed: Option<proto::Session>) {
        self.emit_all(events);
        if let Some(session) = changed {
            self.emit(Event::SessionChanged { session });
        }
    }

    pub fn create(self: &Arc<Self>, params: proto::CreateParams) -> Result<proto::Session, Fail> {
        if self.is_stopping() {
            return Err((ErrorCode::Internal, "server stopping".into()));
        }
        let limits = &self.config.limits;
        let label = params.label.unwrap_or_default();
        if label.len() > proto::cap(limits.label) {
            return Err((ErrorCode::LimitExceeded, "label too long".into()));
        }
        let argv = params.argv.unwrap_or_else(|| self.config.shell.clone());
        let cwd = params
            .cwd
            .unwrap_or_else(|| self.config.home.to_string_lossy().into_owned());
        check_command(&argv, &cwd, params.env.as_ref(), limits)?;
        let cols = params.cols.unwrap_or(80);
        let rows = params.rows.unwrap_or(24);
        check_size(&proto::Size {
            cols,
            rows,
            cell_width: None,
            cell_height: None,
        })?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut env = BTreeMap::from([("TERM".to_string(), "xterm-ghostty".to_string())]);
        env.extend(params.env.unwrap_or_default());
        env.insert("CLANKERMUX_SESSION".into(), id.clone());
        env.insert(
            "CLANKERMUX_SOCKET".into(),
            self.config.socket.to_string_lossy().into_owned(),
        );

        let term = new_terminal(cols, rows, limits, &self.config.version).map_err(internal)?;
        let (pty, child) = pty::spawn(&argv, std::path::Path::new(&cwd), &env, cols, rows)
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
        let (input, input_rx) = InputQueue::new(proto::cap(limits.input_queue));
        let created = Instant::now();
        let body = Body::Live(Box::new(Live {
            pid: Some(pid),
            pgid: pid,
            term,
            stream: Stream::new(proto::cap(limits.ring), proto::cap(limits.markers)),
            master: master.clone(),
            slave_path: pty.slave_path,
            input,
            pty_eof: false,
            pending: None,
            timer_running: false,
            prompts: VecDeque::new(),
            last_output: created,
        }));
        let session = Session {
            id,
            label,
            argv,
            cwd,
            on_exit: params.on_exit.unwrap_or(OnExit::Remove),
            created_at: now(),
            unparked: Some(Unparked::at(created)),
            ..Session::blank(limits, body)
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
        self.emit(Event::SessionCreated {
            session: snapshot.clone(),
        });
        tokio::spawn(park_when_due(
            Arc::downgrade(self),
            Arc::downgrade(&shared),
            due,
            self.shutdown.clone(),
        ));
        tokio::spawn(read_pty(self.clone(), shared.clone(), master.clone()));
        tokio::spawn(wait_child(self.clone(), shared, child));
        tokio::spawn(write_input(master, input_rx));
        Ok(snapshot)
    }

    fn ended(self: &Arc<Self>, session: &Shared, ended: Option<proto::Session>) {
        let Some(snapshot) = ended else { return };
        let events = session.lock().unwrap().take_events();
        self.emit_all(events);
        let remove = snapshot.on_exit == OnExit::Remove;
        let id = snapshot.id.clone();
        self.emit(Event::SessionExited { session: snapshot });
        if self.is_stopping() {
            return;
        }
        if remove {
            park::remove(&self.sessions_dir(), &id);
            self.remove_after_grace(id);
        } else {
            let registry = self.clone();
            let session = session.clone();
            tokio::spawn(async move { registry.park(&session).await });
        }
    }

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
                s.removed = true;
                s.close_finished_attachments(ClosedReason::Removed);
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

    fn remove_now(&self, id: &str) {
        let Some(session) = self.state.lock().unwrap().sessions.remove(id) else {
            return;
        };
        {
            let mut s = session.lock().unwrap();
            s.removed = true;
            s.close_attachments(ClosedReason::Removed);
            s.wake.notify_waiters();
        }
        park::remove(&self.sessions_dir(), id);
        self.emit(Event::SessionRemoved { id: id.to_string() });
    }
}

fn check_command(
    argv: &[String],
    cwd: &str,
    env: Option<&BTreeMap<String, String>>,
    limits: &Limits,
) -> Result<(), Fail> {
    if cwd.len() > proto::cap(limits.text)
        || argv.iter().map(String::len).sum::<usize>() > proto::cap(limits.text)
    {
        return Err((ErrorCode::LimitExceeded, "cwd or argv too long".into()));
    }
    if argv.is_empty()
        || argv
            .iter()
            .map(String::as_str)
            .chain([cwd])
            .any(|s| s.contains('\0'))
        || env
            .into_iter()
            .flatten()
            .any(|(k, v)| k.contains('\0') || v.contains('\0'))
    {
        return Err((
            ErrorCode::InvalidRequest,
            "argv must not be empty, and argv, cwd and env must not hold NUL".into(),
        ));
    }
    Ok(())
}

fn new_terminal(
    cols: u16,
    rows: u16,
    limits: &Limits,
    version: &str,
) -> ghostty_vt::Result<ghostty_vt::Terminal<Effects>> {
    let mut term = ghostty_vt::Terminal::with_handler(cols, rows, Effects::default())?;
    term.set_xtversion(&format!("clankermux {version}"));
    term.set_terminfo_name("xterm-ghostty")?;
    for kind in [
        ghostty_vt::ColorKind::Foreground,
        ghostty_vt::ColorKind::Background,
        ghostty_vt::ColorKind::Cursor,
    ] {
        term.set_color(kind, Some(default_color(kind)))?;
    }
    term.set_scrollback_max_bytes(proto::cap(limits.scrollback))?;
    term.set_continuation_max_bytes(proto::cap(limits.continuation))?;
    term.handler_mut().size = ghostty_vt::SizeReport {
        cols,
        rows,
        cell_width: None,
        cell_height: None,
    };
    Ok(term)
}

async fn read_pty(registry: Arc<Registry>, session: Shared, master: Arc<AsyncFd<OwnedFd>>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let Ok(mut guard) = master.readable().await else {
            break;
        };
        match guard
            .try_io(|fd| rustix::io::read(fd.get_ref(), &mut buf).map_err(std::io::Error::from))
        {
            // EOF, or EIO: how Linux reports the slave side closing.
            Ok(Ok(0) | Err(_)) => break,
            Ok(Ok(n)) => {
                let events = {
                    let mut s = session.lock().unwrap();
                    s.output(&buf[..n]);
                    s.take_events()
                };
                registry.emit_all(events);
            }
            Err(_would_block) => {}
        }
    }
    let ended = {
        let mut s = session.lock().unwrap();
        if let Some(live) = s.live_mut() {
            live.pty_eof = true;
        }
        s.maybe_end().map(|_| s.to_proto(false))
    };
    registry.ended(&session, ended);
}

async fn wait_child(registry: Arc<Registry>, session: Shared, mut child: tokio::process::Child) {
    let status = child.wait().await;
    let ended = {
        let mut s = session.lock().unwrap();
        if let Some(live) = s.live_mut() {
            live.pid = None;
        }
        s.exit = Some(match status {
            Ok(status) => exit_of(status),
            Err(_) => Exit::Code { code: -1 },
        });
        s.maybe_end().map(|_| s.to_proto(false))
    };
    registry.ended(&session, ended);
}

async fn write_input(master: Arc<AsyncFd<OwnedFd>>, mut rx: mpsc::UnboundedReceiver<InputUnit>) {
    while let Some(InputUnit {
        bytes,
        permit,
        done,
    }) = rx.recv().await
    {
        let mut written = 0;
        while written < bytes.len() {
            let Ok(mut guard) = master.writable().await else {
                return;
            };
            match guard.try_io(|fd| {
                rustix::io::write(fd.get_ref(), &bytes[written..]).map_err(std::io::Error::from)
            }) {
                Ok(Ok(n)) => written += n,
                Ok(Err(_)) => return,
                Err(_would_block) => {}
            }
        }
        drop(permit);
        let _ = done.send(());
    }
}

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
                match s.unparked {
                    Some(changes) => registry.config.park.next_due(changes, s.parked_at),
                    None => break,
                }
            };
            if Instant::now() >= deadline {
                registry.park_if(&shared, true).await;
                break;
            }
            drop((registry, shared));
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {}
                () = shutdown.cancelled() => return,
            }
        }
        tokio::select! {
            () = due.notified() => {}
            () = shutdown.cancelled() => return,
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

#[cfg(test)]
mod tests {
    use super::*;

    const TIMING: ParkTiming = ParkTiming {
        interval: Duration::from_secs(300),
        quiet: Duration::from_secs(2),
        gap: Duration::from_secs(5),
        busy: Duration::from_secs(30),
    };

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_change_is_due_once_quiet() {
        let t0 = Instant::now();
        let changes = Unparked {
            first: t0,
            latest: t0 + secs(1),
        };
        assert_eq!(TIMING.next_due(changes, None), t0 + secs(3));
    }

    #[test]
    fn continuing_changes_are_due_when_busy() {
        let t0 = Instant::now();
        let changes = Unparked {
            first: t0,
            latest: t0 + secs(29),
        };
        assert_eq!(TIMING.next_due(changes, None), t0 + secs(30));
    }

    #[test]
    fn a_recent_park_holds_the_next_off_for_the_gap() {
        let t0 = Instant::now();
        assert_eq!(
            TIMING.next_due(Unparked::at(t0 + secs(1)), Some(t0)),
            t0 + secs(5)
        );
        assert_eq!(
            TIMING.next_due(Unparked::at(t0 + secs(10)), Some(t0)),
            t0 + secs(12)
        );
    }
}
