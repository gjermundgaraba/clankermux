//! All sessions and the subscribers to their events.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use proto::{ErrorCode, Event, Exit, FrameType, Limits, OnExit, Page, Status};
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;

use crate::connection::Writer;
use crate::session::{Effects, InputQueue, Session, Shared, now};
use crate::stream::Stream;
use crate::{frame_json, pty};

pub type Fail = (ErrorCode, String);

pub struct Config {
    pub limits: Limits,
    /// Default `argv`.
    pub shell: Vec<String>,
    /// Default `cwd`.
    pub home: PathBuf,
    pub version: String,
}

pub struct Registry {
    pub config: Config,
    pub incarnation: String,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<String, Shared>,
    subscribers: Vec<Writer>,
}

impl Registry {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Registry {
            config,
            incarnation: uuid::Uuid::new_v4().to_string(),
            state: Mutex::default(),
        })
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
            pid: Some(pid),
            pgid: pid,
            on_exit: params.on_exit.unwrap_or(OnExit::Remove),
            created_at: now(),
            exited_at: None,
            exit: None,
            term,
            stream: Stream::new(limits.ring as usize, limits.markers as usize),
            wake: Arc::new(Notify::new()),
            master: master.clone(),
            slave_path: pty.slave_path,
            input,
            pty_eof: false,
            text_limit: limits.text as usize,
            attached: Vec::new(),
            pending: None,
            timer_running: false,
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
                        let changed = {
                            let mut session = reader_session.lock().unwrap();
                            session.output(&buf[..n]).then(|| session.to_proto(false))
                        };
                        if let Some(session) = changed {
                            registry.emit(&Event::SessionChanged { session });
                        }
                    }
                    // EIO is how Linux reports the slave side closing.
                    Ok(Err(_)) => break,
                    Err(_would_block) => continue,
                }
            }
            let ended = {
                let mut session = reader_session.lock().unwrap();
                session.pty_eof = true;
                session.maybe_end().map(|_| session.to_proto(false))
            };
            registry.ended(ended);
        });

        let registry = self.clone();
        let wait_session = shared.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            let ended = {
                let mut session = wait_session.lock().unwrap();
                session.pid = None;
                session.exit = Some(match status {
                    Ok(status) => exit_of(status),
                    Err(_) => Exit::Code { code: -1 },
                });
                session.maybe_end().map(|_| session.to_proto(false))
            };
            registry.ended(ended);
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

    /// Emits `session_exited` and schedules removal when a stream ended.
    fn ended(self: &Arc<Self>, ended: Option<proto::Session>) {
        let Some(snapshot) = ended else { return };
        let remove = snapshot.on_exit == OnExit::Remove;
        let id = snapshot.id.clone();
        self.emit(&Event::SessionExited { session: snapshot });
        if remove {
            self.remove_after_grace(id);
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
                s.close_attachments(true);
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

    /// Drops the session, closing every attachment.
    fn remove_now(&self, id: &str) {
        let Some(session) = self.state.lock().unwrap().sessions.remove(id) else {
            return;
        };
        session.lock().unwrap().close_attachments(false);
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
