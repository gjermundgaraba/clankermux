//! One client connection: framing, handshake, dispatch and the channel
//! table.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use proto::{
    ErrorCode, ErrorMessage, FrameDecoder, FrameType, Header, Hello, Open, Viewport, Welcome,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::attach::{self, Handle};
use crate::registry::Registry;
use crate::session::{self, Report, Shared};
use crate::{control, frame_json};

/// Queued bytes above which attachments stop pulling from their streams
/// until the socket drains, so a slow client falls behind in the ring
/// instead of growing server memory.
pub const HIGH_WATER: usize = 256 * 1024;

/// Queued bytes above which the connection is closed: a client that stops
/// reading altogether would otherwise grow its queue of events and
/// replies without bound. Attachments may legitimately overshoot the
/// high-water mark by a few frames.
pub fn ceiling(max_frame: u64) -> usize {
    HIGH_WATER + 4 * max_frame as usize
}

/// The connection's outgoing frame queue, shared by control, events and
/// attachments.
#[derive(Clone)]
pub struct Writer {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    drained: Arc<Notify>,
    ceiling: usize,
    overflow: Arc<Notify>,
}

impl Writer {
    /// Queues a frame. False once the connection is gone.
    pub fn send(&self, frame: Vec<u8>) -> bool {
        let queued = self.queued.fetch_add(frame.len(), Ordering::AcqRel) + frame.len();
        if queued > self.ceiling {
            self.overflow.notify_one();
        }
        self.tx.send(frame).is_ok()
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
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

pub async fn serve_connection<S: AsyncRead + AsyncWrite + Send + 'static>(
    stream: S,
    registry: Arc<Registry>,
) {
    let (mut reader, mut socket) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = Writer {
        tx,
        queued: Arc::new(AtomicUsize::new(0)),
        drained: Arc::new(Notify::new()),
        ceiling: ceiling(registry.config.limits.max_frame),
        overflow: Arc::new(Notify::new()),
    };
    let queued = writer.queued.clone();
    let drained = writer.drained.clone();
    let overflow = writer.overflow.clone();
    // Subscribers hold clones of the sender, so the writer ends on this
    // signal rather than on the channel closing. It says whether to flush
    // the queue first; not after an overflow, when the client is not
    // reading.
    let (close_tx, mut close_rx) = oneshot::channel::<bool>();
    let write_task = tokio::spawn(async move {
        loop {
            let bytes = tokio::select! {
                bytes = rx.recv() => match bytes {
                    Some(bytes) => bytes,
                    None => break,
                },
                flush = &mut close_rx => {
                    rx.close();
                    while flush == Ok(true)
                        && let Ok(bytes) = rx.try_recv()
                    {
                        if socket.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    let _ = socket.flush().await;
                    break;
                }
            };
            if socket.write_all(&bytes).await.is_err() || socket.flush().await.is_err() {
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
    let hello_deadline = tokio::time::Instant::now()
        + Duration::from_millis(state.registry.config.limits.hello_timeout);
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
            _ = overflow.notified() => {
                tracing::warn!("client stopped reading; closing");
                flush = false;
                break;
            }
        };
        match n {
            Ok(0) | Err(_) => break,
            Ok(n) => state.decoder.push(&buf[..n]),
        }
        if !state.drain().await {
            break;
        }
    }
    // Dropping the channel table ends every attachment silently.
    drop(state);
    let _ = close_tx.send(flush);
    let _ = write_task.await;
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
    fn send<T: serde::Serialize>(&self, frame_type: FrameType, channel: u32, body: &T) {
        self.writer.send_json(frame_type, channel, body);
    }

    fn error(&self, channel: u32, code: ErrorCode, message: impl Into<String>) {
        self.send(
            FrameType::Error,
            channel,
            &ErrorMessage {
                id: None,
                code,
                message: message.into(),
            },
        );
    }

    /// Handles every buffered frame; false closes the connection.
    async fn drain(&mut self) -> bool {
        let limits = self.registry.config.limits;
        loop {
            let (header, payload) = match self.decoder.next_frame(limits.max_frame) {
                Ok(Some(frame)) => frame,
                Ok(None) => return true,
                Err(err) => {
                    self.error(0, ErrorCode::ProtocolError, err.to_string());
                    return false;
                }
            };
            let Header {
                frame_type,
                channel,
                ..
            } = header;
            let json_client = !frame_type.is_server() && frame_type.is_json();
            let misplaced = if channel == 0 {
                !frame_type.is_control()
            } else {
                frame_type.is_control()
            };
            if frame_type.is_server()
                || misplaced
                || (json_client && payload.len() > limits.control_body as usize)
            {
                self.error(
                    0,
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
                    self.error(0, ErrorCode::ProtocolError, "second HELLO");
                    return false;
                }
                if !self.hello(&payload) {
                    return false;
                }
                continue;
            }
            if self.hello.is_none() {
                self.error(0, ErrorCode::ProtocolError, "frame before HELLO");
                return false;
            }
            match frame_type {
                FrameType::Request => {
                    if !self.request(&payload).await {
                        return false;
                    }
                }
                FrameType::Open => self.open(channel, &payload),
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
        let raw: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(raw) => raw,
            Err(_) => {
                self.error(0, ErrorCode::ProtocolError, "malformed HELLO");
                return false;
            }
        };
        if raw.get("version").and_then(|v| v.as_u64()) != Some(u64::from(proto::VERSION)) {
            self.error(
                0,
                ErrorCode::VersionMismatch,
                format!("version {} is not {}", raw["version"], proto::VERSION),
            );
            return false;
        }
        let Ok(hello) = serde_json::from_value::<Hello>(raw) else {
            self.error(0, ErrorCode::ProtocolError, "malformed HELLO");
            return false;
        };
        self.hello = Some(hello);
        self.send(
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
                let id = serde_json::from_slice::<serde_json::Value>(payload)
                    .ok()
                    .and_then(|v| v.get("id")?.as_i64());
                self.send(
                    FrameType::Error,
                    0,
                    &ErrorMessage {
                        id,
                        code: ErrorCode::InvalidRequest,
                        message: format!("malformed request: {err}"),
                    },
                );
                return id.is_some();
            }
        };
        let id = request.id;
        let outcome =
            control::handle(&self.registry, &self.writer, &mut self.subscribed, request).await;
        match outcome {
            Ok(result) => self.send(FrameType::Reply, 0, &proto::Reply { id, result }),
            Err((code, message)) => self.send(
                FrameType::Error,
                0,
                &ErrorMessage {
                    id: Some(id),
                    code,
                    message,
                },
            ),
        }
        true
    }

    /// The attachment on `channel`, if the channel is in use.
    fn handle(&self, channel: u32) -> Option<&Handle> {
        self.channels.get(&channel).filter(|h| h.is_open())
    }

    /// Sends an `ERROR` that ends the attachment on `channel`, if any.
    fn end(&mut self, channel: u32, code: ErrorCode, message: String) {
        // Dropping the handle ends the task; the ERROR goes out first.
        self.channels.remove(&channel);
        self.error(channel, code, message);
    }

    fn open(&mut self, channel: u32, payload: &[u8]) {
        // The channel stays in use until the task has queued CLOSED, so a
        // pipelined CLOSE + OPEN is refused and, per the Errors rule, the
        // attachment that was there ends.
        if self.handle(channel).is_some() {
            self.end(channel, ErrorCode::ChannelInUse, "channel in use".into());
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

    /// The session of the writable, running attachment on `channel`; an
    /// error keeps the attachment and drops the frame. None when the
    /// channel is not in use.
    fn writable(&self, channel: u32) -> Option<Shared> {
        let handle = self.handle(channel)?;
        if handle.read_only {
            self.error(channel, ErrorCode::ReadOnly, "attachment is read-only");
            return None;
        }
        let session = handle.session()?;
        if let Err((code, message)) = session.lock().unwrap().running() {
            self.error(channel, code, message);
            return None;
        }
        Some(session)
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
        session::report(&self.registry, &session, Report { viewport }, false);
    }
}
