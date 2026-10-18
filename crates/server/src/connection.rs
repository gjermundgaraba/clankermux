//! One client connection: framing, handshake, dispatch and the channel
//! table.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use proto::{
    ErrorCode, ErrorMessage, FrameDecoder, FrameType, Header, Hello, Open, Viewport, Welcome,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::attach::{self, Handle};
use crate::registry::{Fail, Registry};
use crate::session::{self, Shared};
use crate::{control, frame_json};

/// Queued bytes above which attachments stop pulling from their streams
/// until the socket drains, so a slow client falls behind in the ring
/// instead of growing server memory.
pub const HIGH_WATER: usize = 256 * 1024;

/// How long a closing connection may take to flush its queue.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// The connection's outgoing frame queue, shared by control, events and
/// attachments.
#[derive(Clone)]
pub struct Writer {
    /// Frames in order; the flag marks the last one, after which the
    /// writer stops whatever else was queued.
    tx: mpsc::UnboundedSender<(Vec<u8>, bool)>,
    queued: Arc<AtomicUsize>,
    drained: Arc<Notify>,
    /// Queued bytes above which the connection is closed: a client that
    /// stops reading altogether would otherwise grow its queue of events
    /// and replies without bound. Attachments may legitimately overshoot
    /// the high-water mark by a few frames.
    ceiling: usize,
    overflow: Arc<Notify>,
    /// Set once the last frame is queued (an id-less `ERROR`, or the
    /// ceiling was passed): nothing is accepted after it.
    closing: Arc<AtomicBool>,
}

impl Writer {
    /// Queues a frame. False once the connection is gone or closing.
    pub fn send(&self, frame: Vec<u8>) -> bool {
        if self.closing.load(Ordering::Acquire) {
            return false;
        }
        let queued = self.queued.fetch_add(frame.len(), Ordering::AcqRel) + frame.len();
        if queued > self.ceiling {
            self.closing.store(true, Ordering::Release);
            self.overflow.notify_one();
        }
        self.tx.send((frame, false)).is_ok()
    }

    /// Queues the frame that precedes closing the connection. A frame
    /// another task queues meanwhile lands behind it and is not written.
    pub fn send_last(&self, frame: Vec<u8>) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.tx.send((frame, true));
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
        self.send(frame_json(frame_type, channel, body))
    }

    pub fn over_high_water(&self) -> bool {
        self.queued.load(Ordering::Acquire) > HIGH_WATER
    }

    /// Resolves once the queue is at or below the high-water mark.
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

/// Serves one connection from `accepted` on: `HELLO` is due within
/// `hello_timeout` of that instant.
pub async fn serve_connection<S: AsyncRead + AsyncWrite + Send + 'static>(
    stream: S,
    registry: Arc<Registry>,
    accepted: tokio::time::Instant,
) {
    let (mut reader, mut socket) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel::<(Vec<u8>, bool)>();
    let writer = Writer {
        tx,
        queued: Arc::new(AtomicUsize::new(0)),
        drained: Arc::new(Notify::new()),
        ceiling: HIGH_WATER + 4 * registry.config.limits.max_frame as usize,
        overflow: Arc::new(Notify::new()),
        closing: Arc::new(AtomicBool::new(false)),
    };
    let queued = writer.queued.clone();
    let drained = writer.drained.clone();
    let overflow = writer.overflow.clone();
    let shutdown = registry.shutdown.clone();
    // Subscribers hold clones of the sender, so the writer ends on this
    // signal rather than on the channel closing: it flushes what is
    // queued, then closes.
    let (close_tx, mut close_rx) = oneshot::channel::<()>();
    let mut write_task = tokio::spawn(async move {
        loop {
            let (bytes, last) = tokio::select! {
                next = rx.recv() => match next {
                    Some(next) => next,
                    None => break,
                },
                _ = &mut close_rx => {
                    rx.close();
                    while let Ok((bytes, last)) = rx.try_recv() {
                        if socket.write_all(&bytes).await.is_err() || last {
                            break;
                        }
                    }
                    let _ = socket.flush().await;
                    break;
                }
            };
            if socket.write_all(&bytes).await.is_err() || socket.flush().await.is_err() || last {
                break;
            }
            if queued.fetch_sub(bytes.len(), Ordering::AcqRel) - bytes.len() <= HIGH_WATER {
                drained.notify_waiters();
            }
        }
        let _ = socket.shutdown().await;
    });
    let mut state = Connection {
        registry,
        writer,
        decoder: FrameDecoder::default(),
        hello: None,
        subscribed: false,
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
            // A client that is not reading is not flushed to: the writer
            // may be stuck in a write.
            _ = overflow.notified() => {
                tracing::warn!("client stopped reading; closing");
                flush = false;
                break;
            }
            // The stop has ended every attachment; their `CLOSED` is
            // queued.
            _ = shutdown.cancelled() => break,
        };
        match n {
            Ok(0) => break,
            // A carrier found the bytes malformed (`ws::WsStream`).
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                state.fatal(ErrorCode::ProtocolError, e.to_string());
                break;
            }
            Err(_) => break,
            Ok(n) => state.decoder.push(&buf[..n]),
        }
        if !state.drain().await {
            break;
        }
    }
    // Dropping the channel table ends every attachment silently.
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

/// Sends the reply or error that answers request `id`.
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
    channels: HashMap<u32, Handle>,
}

impl Connection {
    fn error(&self, channel: u32, code: ErrorCode, message: impl Into<String>) {
        self.writer.send_json(
            FrameType::Error,
            channel,
            &ErrorMessage {
                id: None,
                code,
                message: message.into(),
            },
        );
    }

    /// Sends the `ERROR` that precedes closing the connection; nothing is
    /// queued after it.
    fn fatal(&self, code: ErrorCode, message: impl Into<String>) {
        self.writer.send_last(frame_json(
            FrameType::Error,
            0,
            &ErrorMessage {
                id: None,
                code,
                message: message.into(),
            },
        ));
    }

    /// Handles every buffered frame; false closes the connection.
    async fn drain(&mut self) -> bool {
        let limits = self.registry.config.limits;
        loop {
            let (header, payload) = match self.decoder.next_frame(limits.max_frame) {
                Ok(Some(frame)) => frame,
                Ok(None) => return true,
                Err(err) => {
                    self.fatal(ErrorCode::ProtocolError, err.to_string());
                    return false;
                }
            };
            let Header {
                frame_type,
                channel,
                ..
            } = header;
            let misplaced = if channel == 0 {
                !frame_type.is_control()
            } else {
                frame_type.is_control()
            };
            if frame_type.is_server()
                || misplaced
                || (frame_type.is_json() && payload.len() > limits.control_body as usize)
            {
                self.fatal(
                    ErrorCode::ProtocolError,
                    format!(
                        "{frame_type:?} on channel {channel} with {} bytes",
                        payload.len()
                    ),
                );
                return false;
            }
            if frame_type == FrameType::Hello {
                if self.hello.is_some() {
                    self.fatal(ErrorCode::ProtocolError, "second HELLO");
                    return false;
                }
                if !self.hello(&payload) {
                    return false;
                }
                continue;
            }
            if self.hello.is_none() {
                self.fatal(ErrorCode::ProtocolError, "frame before HELLO");
                return false;
            }
            match frame_type {
                FrameType::Request => {
                    if !self.request(&payload).await {
                        return false;
                    }
                }
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

    /// Handles HELLO; false closes the connection.
    fn hello(&mut self, payload: &[u8]) -> bool {
        // The version is read before the typed parse so that another
        // version's shape still gets `version_mismatch`.
        let raw = match serde_json::from_slice::<serde_json::Value>(payload) {
            Ok(raw) if raw.is_object() => raw,
            _ => {
                self.fatal(ErrorCode::ProtocolError, "malformed HELLO");
                return false;
            }
        };
        if raw.get("version").and_then(|v| v.as_u64()) != Some(u64::from(proto::VERSION)) {
            self.fatal(
                ErrorCode::VersionMismatch,
                format!("version {} is not {}", raw["version"], proto::VERSION),
            );
            return false;
        }
        let Ok(hello) = serde_json::from_value::<Hello>(raw) else {
            self.fatal(ErrorCode::ProtocolError, "malformed HELLO");
            return false;
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
        true
    }

    /// Handles one request; false closes the connection.
    async fn request(&mut self, payload: &[u8]) -> bool {
        let request: proto::Request = match serde_json::from_slice(payload) {
            Ok(request) => request,
            Err(err) => {
                // Without an integer id there is no request to answer, and
                // an error without id precedes closing the connection.
                let message = format!("malformed request: {err}");
                match serde_json::from_slice::<serde_json::Value>(payload)
                    .ok()
                    .and_then(|v| v.get("id")?.as_i64())
                {
                    Some(id) => {
                        reply(&self.writer, id, Err((ErrorCode::InvalidRequest, message)));
                        return true;
                    }
                    None => {
                        self.fatal(ErrorCode::InvalidRequest, message);
                        return false;
                    }
                }
            }
        };
        control::dispatch(&self.registry, &self.writer, &mut self.subscribed, request);
        true
    }

    /// The attachment on `channel`, if the channel is in use.
    fn handle(&self, channel: u32) -> Option<&Handle> {
        self.channels.get(&channel).filter(|h| h.is_open())
    }

    /// Ends the attachment on `channel`, if any, with `ERROR code` after
    /// its pending frames.
    fn end(&mut self, channel: u32, code: ErrorCode, message: String) {
        let taken = self
            .handle(channel)
            .is_some_and(|h| h.end(code, message.clone()));
        if !taken {
            self.error(channel, code, message);
        }
    }

    async fn open(&mut self, channel: u32, payload: &[u8]) {
        // The channel stays in use until the task has queued CLOSED, so a
        // pipelined CLOSE + OPEN is refused and, per the Errors rule, the
        // attachment that was there ends. A task that is ending is waited
        // for, so its last frame precedes the new attachment's first.
        if let Some(previous) = self.channels.remove(&channel) {
            if previous.is_open() && !previous.finished() {
                if previous.end(ErrorCode::ChannelInUse, "channel in use".into()) {
                    self.channels.insert(channel, previous);
                } else {
                    self.error(channel, ErrorCode::ChannelInUse, "channel in use");
                }
                return;
            }
            previous.join().await;
        }
        self.channels.retain(|_, h| !h.finished());
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
        let engine = self.hello.as_ref().and_then(|h| h.engine.as_deref());
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
        let handle = attach::open(&self.registry, &self.writer, channel, open, session);
        self.channels.insert(channel, handle);
    }

    /// The session of the writable attachment on `channel`; `read_only`
    /// keeps the attachment and drops the frame. None when the channel is
    /// not in use.
    fn writable(&self, channel: u32) -> Option<Shared> {
        let handle = self.handle(channel)?;
        if handle.read_only {
            self.error(channel, ErrorCode::ReadOnly, "attachment is read-only");
            return None;
        }
        handle.session()
    }

    fn input(&mut self, channel: u32, payload: &[u8]) {
        let Some(session) = self.writable(channel) else {
            return;
        };
        let pushed = session
            .lock()
            .unwrap()
            .running()
            .map(|live| live.input.push(payload.to_vec()));
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
        let Some(session) = self.writable(channel) else {
            return;
        };
        if let Err((code, message)) = session.lock().unwrap().running() {
            self.error(channel, code, message);
            return;
        }
        session::report(&self.registry, &session, viewport, false);
    }
}
