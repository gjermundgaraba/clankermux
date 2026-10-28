use std::collections::HashMap;
use std::io;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use proto::{
    ErrorCode, ErrorMessage, FrameDecoder, FrameType, Header, Hello, Open, Viewport, Welcome,
    encode_json_frame,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::attach::{self, ChannelState, Handle};
use crate::control;
use crate::registry::{Fail, Registry};
use crate::session::{self, Shared};

pub const HIGH_WATER: usize = 256 * 1024;

const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

enum Outgoing {
    Frame(Vec<u8>),
    Last(Vec<u8>),
}

enum Writable {
    Ok { id: u64, session: Shared },
    Missing,
    ReadOnly,
}

impl Outgoing {
    fn bytes(&self) -> &[u8] {
        match self {
            Outgoing::Frame(bytes) | Outgoing::Last(bytes) => bytes,
        }
    }

    fn is_last(&self) -> bool {
        matches!(self, Outgoing::Last(_))
    }
}

#[derive(Clone)]
pub struct Writer {
    tx: mpsc::UnboundedSender<Outgoing>,
    queued: Arc<AtomicUsize>,
    drained: Arc<Notify>,
    ceiling: usize,
    overflow: Arc<Notify>,
    closing: Arc<AtomicBool>,
}

impl Writer {
    pub fn send(&self, frame: Vec<u8>) -> bool {
        if self.closing.load(Ordering::Acquire) {
            return false;
        }
        let queued = self.queued.fetch_add(frame.len(), Ordering::AcqRel) + frame.len();
        if queued > self.ceiling {
            self.closing.store(true, Ordering::Release);
            self.overflow.notify_one();
        }
        self.tx.send(Outgoing::Frame(frame)).is_ok()
    }

    pub fn send_last(&self, frame: Vec<u8>) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.tx.send(Outgoing::Last(frame));
    }

    pub fn is_closed(&self) -> bool {
        self.closing.load(Ordering::Acquire) || self.tx.is_closed()
    }

    pub fn send_json<T: serde::Serialize>(
        &self,
        frame_type: FrameType,
        channel: u32,
        body: &T,
    ) -> bool {
        self.send(encode_json_frame(frame_type, channel, body))
    }

    pub fn over_high_water(&self) -> bool {
        self.queued.load(Ordering::Acquire) > HIGH_WATER
    }

    pub async fn drained(&self) {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.over_high_water() {
                return;
            }
            notified.await;
        }
    }
}

pub async fn serve_connection<S: AsyncRead + AsyncWrite + Send + 'static>(
    stream: S,
    registry: Arc<Registry>,
    accepted: tokio::time::Instant,
) {
    let (mut reader, socket) = tokio::io::split(stream);
    let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
    let writer = Writer {
        tx,
        queued: Arc::new(AtomicUsize::new(0)),
        drained: Arc::new(Notify::new()),
        ceiling: HIGH_WATER + 4 * proto::cap(registry.config.limits.max_frame),
        overflow: Arc::new(Notify::new()),
        closing: Arc::new(AtomicBool::new(false)),
    };
    let overflow = writer.overflow.clone();
    let shutdown = registry.shutdown.clone();
    let (close_tx, close_rx) = oneshot::channel::<()>();
    let mut write_task = tokio::spawn(write_queued(
        socket,
        rx,
        writer.queued.clone(),
        writer.drained.clone(),
        close_rx,
    ));
    let mut state = Connection {
        registry,
        writer,
        decoder: FrameDecoder::default(),
        hello: None,
        subscribed: false,
        pending: control::Pending::default(),
        channels: HashMap::new(),
    };
    let mut buf = vec![0u8; 64 * 1024];
    let hello_deadline =
        accepted + Duration::from_millis(state.registry.config.limits.hello_timeout);
    let mut flush = true;
    loop {
        let read = async {
            if state.hello.is_some() {
                reader.read(&mut buf).await
            } else {
                match tokio::time::timeout_at(hello_deadline, reader.read(&mut buf)).await {
                    Ok(n) => n,
                    Err(_) => Ok(0),
                }
            }
        };
        let n = tokio::select! {
            n = read => n,
            () = overflow.notified() => {
                tracing::warn!("client stopped reading; closing");
                flush = false;
                break;
            }
            () = shutdown.cancelled() => break,
        };
        match n {
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                state.fatal(ErrorCode::ProtocolError, e.to_string());
                break;
            }
            Ok(0) | Err(_) => break,
            Ok(n) => state.decoder.push(&buf[..n]),
        }
        if state.drain().await.is_break() {
            break;
        }
    }
    state.pending.lock().unwrap().clear();
    drop(state);
    if flush {
        let _ = close_tx.send(());
        if tokio::time::timeout(FLUSH_TIMEOUT, &mut write_task)
            .await
            .is_ok()
        {
            return;
        }
    }
    write_task.abort();
}

async fn write_queued<W: AsyncWrite + Unpin>(
    mut socket: W,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
    queued: Arc<AtomicUsize>,
    drained: Arc<Notify>,
    mut close: oneshot::Receiver<()>,
) {
    loop {
        let outgoing = tokio::select! {
            next = rx.recv() => match next {
                Some(next) => next,
                None => break,
            },
            _ = &mut close => {
                rx.close();
                while let Ok(outgoing) = rx.try_recv() {
                    if socket.write_all(outgoing.bytes()).await.is_err() || outgoing.is_last() {
                        break;
                    }
                }
                let _ = socket.flush().await;
                break;
            }
        };
        let bytes = outgoing.bytes();
        if socket.write_all(bytes).await.is_err()
            || socket.flush().await.is_err()
            || outgoing.is_last()
        {
            break;
        }
        if queued.fetch_sub(bytes.len(), Ordering::AcqRel) - bytes.len() <= HIGH_WATER {
            drained.notify_waiters();
        }
    }
    let _ = socket.shutdown().await;
}

pub fn error_frame(channel: u32, code: ErrorCode, message: String) -> Vec<u8> {
    encode_json_frame(
        FrameType::Error,
        channel,
        &ErrorMessage {
            id: None,
            code,
            message,
        },
    )
}

pub fn reply(writer: &Writer, id: i64, outcome: Result<serde_json::Value, Fail>) {
    match outcome {
        Ok(result) => writer.send_json(FrameType::Reply, 0, &proto::Reply { id, result }),
        Err((code, message)) => writer.send_json(
            FrameType::Error,
            0,
            &ErrorMessage {
                id: Some(id),
                code,
                message,
            },
        ),
    };
}

struct Connection {
    registry: Arc<Registry>,
    writer: Writer,
    decoder: FrameDecoder,
    hello: Option<Hello>,
    subscribed: bool,
    pending: control::Pending,
    channels: HashMap<u32, Handle>,
}

impl Connection {
    fn error(&self, channel: u32, code: ErrorCode, message: impl Into<String>) {
        self.writer.send(error_frame(channel, code, message.into()));
    }

    fn fatal(&self, code: ErrorCode, message: impl Into<String>) {
        self.writer.send_last(error_frame(0, code, message.into()));
    }

    async fn drain(&mut self) -> ControlFlow<()> {
        let limits = self.registry.config.limits;
        loop {
            let (header, payload) = match self.decoder.next_frame(limits.max_frame) {
                Ok(Some(frame)) => frame,
                Ok(None) => return ControlFlow::Continue(()),
                Err(err) => {
                    self.fatal(ErrorCode::ProtocolError, err.to_string());
                    return ControlFlow::Break(());
                }
            };
            let Header {
                frame_type,
                channel,
                ..
            } = header;
            if frame_type.is_server()
                || !frame_type.allowed_on(channel)
                || (frame_type.is_json() && payload.len() > proto::cap(limits.control_body))
            {
                self.fatal(
                    ErrorCode::ProtocolError,
                    format!(
                        "{frame_type:?} on channel {channel} with {} bytes",
                        payload.len()
                    ),
                );
                return ControlFlow::Break(());
            }
            if frame_type == FrameType::Hello {
                if self.hello.is_some() {
                    self.fatal(ErrorCode::ProtocolError, "second HELLO");
                    return ControlFlow::Break(());
                }
                self.hello(&payload)?;
                continue;
            }
            if self.hello.is_none() {
                self.fatal(ErrorCode::ProtocolError, "frame before HELLO");
                return ControlFlow::Break(());
            }
            match frame_type {
                FrameType::Request => self.request(&payload)?,
                FrameType::Open => self.open(channel, &payload).await,
                FrameType::Input => self.input(channel, &payload),
                FrameType::Viewport => self.viewport(channel, &payload),
                FrameType::Close => {
                    if let Some(handle) = self.handle(channel) {
                        handle.close();
                    }
                }
                _ => {}
            }
        }
    }

    fn hello(&mut self, payload: &[u8]) -> ControlFlow<()> {
        let raw = match serde_json::from_slice::<serde_json::Value>(payload) {
            Ok(raw) if raw.is_object() => raw,
            _ => {
                self.fatal(ErrorCode::ProtocolError, "malformed HELLO");
                return ControlFlow::Break(());
            }
        };
        if raw.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(proto::VERSION))
        {
            self.fatal(
                ErrorCode::VersionMismatch,
                format!("version {} is not {}", raw["version"], proto::VERSION),
            );
            return ControlFlow::Break(());
        }
        let hello = match serde_json::from_value::<Hello>(raw) {
            Ok(hello) => hello,
            Err(err) => {
                self.fatal(ErrorCode::InvalidRequest, format!("bad HELLO: {err}"));
                return ControlFlow::Break(());
            }
        };
        self.hello = Some(hello);
        self.writer.send_json(
            FrameType::Welcome,
            0,
            &Welcome {
                version: proto::VERSION,
                server: proto::Peer {
                    name: "clankermux".into(),
                    version: self.registry.config.version.clone(),
                },
                engine: ghostty_vt::ENGINE.into(),
                incarnation: self.registry.incarnation.clone(),
                capabilities: Vec::new(),
                limits: self.registry.config.limits,
            },
        );
        ControlFlow::Continue(())
    }

    fn request(&mut self, payload: &[u8]) -> ControlFlow<()> {
        let request: proto::Request = match serde_json::from_slice(payload) {
            Ok(request) => request,
            Err(err) => {
                let message = format!("malformed request: {err}");
                if let Some(id) = serde_json::from_slice::<serde_json::Value>(payload)
                    .ok()
                    .and_then(|v| v.get("id")?.as_i64())
                {
                    reply(&self.writer, id, Err((ErrorCode::InvalidRequest, message)));
                    return ControlFlow::Continue(());
                }
                self.fatal(ErrorCode::InvalidRequest, message);
                return ControlFlow::Break(());
            }
        };
        control::dispatch(
            &self.registry,
            &self.writer,
            &self.pending,
            &mut self.subscribed,
            request,
        );
        ControlFlow::Continue(())
    }

    fn handle(&self, channel: u32) -> Option<&Handle> {
        self.channels
            .get(&channel)
            .filter(|h| h.state() == ChannelState::Open)
    }

    fn end(&mut self, channel: u32, code: ErrorCode, message: String) {
        let taken = self
            .handle(channel)
            .is_some_and(|h| h.end(code, message.clone()).is_ok());
        if !taken {
            self.error(channel, code, message);
        }
    }

    async fn open(&mut self, channel: u32, payload: &[u8]) {
        if let Some(previous) = self.channels.remove(&channel) {
            match previous.state() {
                ChannelState::Open => {
                    if previous
                        .end(ErrorCode::ChannelInUse, "channel in use".into())
                        .is_ok()
                    {
                        self.channels.insert(channel, previous);
                    } else {
                        self.error(channel, ErrorCode::ChannelInUse, "channel in use");
                    }
                    return;
                }
                ChannelState::Ending | ChannelState::Finished => previous.join().await,
            }
        }
        self.channels
            .retain(|_, h| h.state() != ChannelState::Finished);
        if self.registry.is_stopping() {
            self.error(channel, ErrorCode::Internal, "server stopping");
            return;
        }
        let open: Open = match serde_json::from_slice(payload) {
            Ok(open) => open,
            Err(err) => {
                self.error(
                    channel,
                    ErrorCode::InvalidRequest,
                    format!("malformed OPEN: {err}"),
                );
                return;
            }
        };
        if let Some(size) = &open.viewport
            && let Err((code, message)) = session::check_size(size)
        {
            self.error(channel, code, message);
            return;
        }
        let session = match self.registry.get(&open.session) {
            Ok(session) => session,
            Err((code, message)) => {
                self.error(channel, code, message);
                return;
            }
        };
        let hello = self.hello.as_ref().expect("OPEN follows HELLO");
        let engine = hello.engine.as_deref();
        if engine != Some(ghostty_vt::ENGINE) {
            self.error(
                channel,
                ErrorCode::EngineMismatch,
                format!("client engine {engine:?} is not {}", ghostty_vt::ENGINE),
            );
            return;
        }
        let parked_engine = session
            .lock()
            .unwrap()
            .parked()
            .map(|p| p.engine.clone())
            .filter(|e| e != ghostty_vt::ENGINE);
        if let Some(parked_engine) = parked_engine {
            self.error(
                channel,
                ErrorCode::EngineMismatch,
                format!("the session was parked by engine {parked_engine}"),
            );
            return;
        }
        match attach::open(
            &self.registry,
            &self.writer,
            channel,
            &hello.client,
            open,
            &session,
        ) {
            Ok(handle) => {
                self.channels.insert(channel, handle);
            }
            Err((code, message)) => self.error(channel, code, message),
        }
    }

    fn writable(&self, channel: u32) -> Writable {
        let Some(handle) = self.handle(channel) else {
            return Writable::Missing;
        };
        if handle.read_only {
            return Writable::ReadOnly;
        }
        match handle.session() {
            Some(session) => Writable::Ok {
                id: handle.id,
                session,
            },
            None => Writable::Missing,
        }
    }

    fn refuse_read_only(&self, channel: u32) {
        self.error(channel, ErrorCode::ReadOnly, "attachment is read-only");
    }

    fn input(&mut self, channel: u32, payload: &[u8]) {
        let (id, session) = match self.writable(channel) {
            Writable::Ok { id, session } => (id, session),
            Writable::ReadOnly => return self.refuse_read_only(channel),
            Writable::Missing => return,
        };
        let pushed = {
            let mut s = session.lock().unwrap();
            let pushed = s.running().map(|live| live.input.push(payload.to_vec()));
            if let Ok(Some(_)) = pushed {
                s.touch();
                if let Some(a) = s.attached.iter_mut().find(|a| a.id == id) {
                    a.last_input = Some(session::now());
                }
            }
            pushed
        };
        match pushed {
            Err((code, message)) => self.error(channel, code, message),
            Ok(None) => self.error(channel, ErrorCode::InputOverflow, "input queue full"),
            Ok(Some(_)) => {}
        }
    }

    fn viewport(&mut self, channel: u32, payload: &[u8]) {
        if self.handle(channel).is_none() {
            return;
        }
        let viewport: Viewport = match serde_json::from_slice(payload) {
            Ok(viewport) => viewport,
            Err(err) => {
                self.end(
                    channel,
                    ErrorCode::InvalidRequest,
                    format!("malformed VIEWPORT: {err}"),
                );
                return;
            }
        };
        if let Err((code, message)) = session::check_size(&viewport.size) {
            self.end(channel, code, message);
            return;
        }
        let (id, session) = match self.writable(channel) {
            Writable::Ok { id, session } => (id, session),
            Writable::ReadOnly => return self.refuse_read_only(channel),
            Writable::Missing => return,
        };
        if let Err((code, message)) = session.lock().unwrap().running() {
            self.error(channel, code, message);
            return;
        }
        session::report_coalesced(&self.registry, &session, id, viewport);
    }
}
