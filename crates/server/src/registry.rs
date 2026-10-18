//! All sessions and the subscribers to their events.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ghostty_vt::snapshot;
use proto::{ClosedReason, ErrorCode, Event, Exit, FrameType, Limits, OnExit, Page, Status};
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;

use crate::connection::Writer;
use crate::park;
use crate::records::Records;
use crate::session::{Body, Effects, InputQueue, Live, Parked, Session, Shared, now};
use crate::stream::Stream;
use crate::{frame_json, pty};

pub type Fail = (ErrorCode, String);

/// Running sessions are parked this often.
const PARK_INTERVAL: Duration = Duration::from_secs(300);
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
    /// Set by `stop`: no more parks from the timer.
    stopping: AtomicBool,
    /// The exclusive lock on the state directory, for the server's life.
    _lock: File,
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
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&config.state_dir)?;
        let lock = File::create(config.state_dir.join("lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "another server holds {}",
                        config.state_dir.join("lock").display()
                    ),
                )
            },
        )?;
        let loaded = park::load(&config.state_dir.join("sessions"))?;
        let registry = Arc::new(Registry {
            config,
            incarnation: uuid::Uuid::new_v4().to_string(),
            state: Mutex::default(),
            stopping: AtomicBool::new(false),
            _lock: lock,
        });
        for loaded in loaded {
            registry.restore(loaded);
        }
        let timer = registry.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PARK_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                if timer.stopping.load(Ordering::Acquire) {
                    return;
                }
                timer.park_running().await;
            }
        });
        Ok(registry)
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
            status: if exited {
                Status::Exited
            } else {
                Status::Restored
            },
            argv: info.argv,
            cwd: info.cwd,
            on_exit: info.on_exit,
            created_at: info.created_at,
            exited_at: info.exited_at,
            exit: info.exit.filter(|_| exited),
            wake: Arc::new(Notify::new()),
            text_limit: limits.text as usize,
            records_limit: limits.records as usize,
            attached: Vec::new(),
            records: Records::load(info.program_status.unwrap_or_default()),
            events: Vec::new(),
            body: Body::Parked(Parked {
                engine: file.engine,
                cols: info.cols,
                rows: info.rows,
                title: info.title,
                pwd: info.pwd,
                offset: info.offset,
                marker: info.marker,
                profile: file.profile,
                cell: file.cell,
                screen,
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
            .filter(|s| s.lock().unwrap().status == Status::Running)
            .cloned()
            .collect();
        for session in running {
            self.park(&session).await;
        }
    }

    /// Writes a generation for a live session, retrying the snapshot while
    /// the continuation limit is exceeded. A failure keeps the previous
    /// generation and is logged.
    pub async fn park(self: &Arc<Self>, session: &Shared) {
        let deadline = Instant::now() + PARK_RETRY;
        let result = loop {
            let wake = session.lock().unwrap().wake.clone();
            let notified = wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let taken = {
                let s = session.lock().unwrap();
                let Some(live) = s.live() else {
                    return;
                };
                let encode = || snapshot::encode(&live.term);
                let encoded = match tokio::runtime::Handle::current().runtime_flavor() {
                    tokio::runtime::RuntimeFlavor::CurrentThread => encode(),
                    _ => tokio::task::block_in_place(encode),
                };
                encoded.and_then(|bytes| s.park_file().map(|(file, screen)| (bytes, file, screen)))
            };
            match taken {
                Ok((bytes, file, screen)) => {
                    let generation = jiff::Timestamp::now().as_millisecond() as u64;
                    break park::write(&self.sessions_dir(), &file, generation, &bytes, &screen)
                        .map_err(|e| e.to_string());
                }
                Err(ghostty_vt::Error::LimitExceeded) if Instant::now() < deadline => {
                    tokio::select! {
                        _ = notified => {}
                        _ = tokio::time::sleep_until(deadline.into()) => {}
                    }
                }
                Err(err) => break Err(err.to_string()),
            }
        };
        if let Err(error) = result {
            let id = session.lock().unwrap().id.clone();
            tracing::warn!(session = id, error, "park failed; previous files kept");
        }
    }

    /// Graceful stop: parks every running session, closes every
    /// attachment with `stopping`, and hangs up each running session's
    /// process group.
    pub async fn stop(self: &Arc<Self>) {
        self.stopping.store(true, Ordering::Release);
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
            if s.status == Status::Running
                && let Some(live) = s.live()
            {
                pty::signal(
                    live.pgid,
                    &live.slave_path,
                    pty::signal_by_name("HUP").unwrap(),
                );
            }
        }
        // The server's lifetime is over; the next one may take the directory.
        let _ = rustix::fs::flock(&self._lock, rustix::fs::FlockOperation::Unlock);
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

    fn page_of(&self, state: &State, after: Option<&str>) -> Page {
        let page = self.config.limits.page as usize;
        let mut sessions: Vec<proto::Session> = state
            .sessions
            .range::<str, _>((
                after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
                std::ops::Bound::Unbounded,
            ))
            .take(page + 1)
            .map(|(_, s)| s.lock().unwrap().to_proto(false))
            .collect();
        let next = (sessions.len() > page).then(|| {
            sessions.truncate(page);
            sessions.last().unwrap().id.clone()
        });
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
            status: Status::Running,
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
            body: Body::Live(Live {
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
            }),
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
    /// or parks the session once for `on_exit: keep`.
    fn ended(self: &Arc<Self>, session: &Shared, ended: Option<proto::Session>) {
        let Some(snapshot) = ended else { return };
        let events = session.lock().unwrap().take_events();
        self.emit_all(events);
        let remove = snapshot.on_exit == OnExit::Remove;
        let id = snapshot.id.clone();
        self.emit(&Event::SessionExited { session: snapshot });
        if remove {
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
            registry.remove_now(&id);
        });
    }

    /// An explicit `remove`: finished attachments close at once; when
    /// others are open the session stays until `remove_grace` has passed.
    pub fn remove(self: &Arc<Self>, id: &str) -> Result<(), Fail> {
        let session = self.get(id)?;
        let all_finished = {
            let s = session.lock().unwrap();
            if s.status == Status::Running {
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
        session
            .lock()
            .unwrap()
            .close_attachments(ClosedReason::Removed, false);
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
