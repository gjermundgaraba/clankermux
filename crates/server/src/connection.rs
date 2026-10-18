//! One client connection: framing, handshake and dispatch.

use std::sync::Arc;
use std::time::Duration;

use proto::{ErrorCode, ErrorMessage, FrameDecoder, FrameType, Header, Hello, Welcome};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::control::{self, Conn};
use crate::frame_json;
use crate::registry::Registry;

pub async fn serve_connection<S: AsyncRead + AsyncWrite + Send + 'static>(
    stream: S,
    registry: Arc<Registry>,
) {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // Subscribers hold clones of `tx`, so the writer ends on this signal
    // rather than on the channel closing.
    let (close_tx, mut close_rx) = oneshot::channel::<()>();
    let write_task = tokio::spawn(async move {
        loop {
            let bytes = tokio::select! {
                bytes = rx.recv() => match bytes {
                    Some(bytes) => bytes,
                    None => break,
                },
                _ = &mut close_rx => {
                    rx.close();
                    while let Ok(bytes) = rx.try_recv() {
                        if writer.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    break;
                }
            };
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });
    let conn = Conn {
        tx: tx.clone(),
        subscribed: false,
    };
    let mut state = Connection {
        registry,
        conn,
        decoder: FrameDecoder::default(),
        hello_done: false,
    };
    let mut buf = vec![0u8; 64 * 1024];
    let hello_deadline = tokio::time::Instant::now()
        + Duration::from_millis(state.registry.config.limits.hello_timeout);
    loop {
        let read = reader.read(&mut buf);
        let n = if state.hello_done {
            read.await
        } else {
            match tokio::time::timeout_at(hello_deadline, read).await {
                Ok(n) => n,
                Err(_) => break,
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
    drop(state);
    drop(tx);
    let _ = close_tx.send(());
    let _ = write_task.await;
}

struct Connection {
    registry: Arc<Registry>,
    conn: Conn,
    decoder: FrameDecoder,
    hello_done: bool,
}

impl Connection {
    fn send<T: serde::Serialize>(&self, frame_type: FrameType, channel: u32, body: &T) {
        let _ = self.conn.tx.send(frame_json(frame_type, channel, body));
    }

    fn fail(&self, code: ErrorCode, message: impl Into<String>) {
        self.send(
            FrameType::Error,
            0,
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
                    self.fail(ErrorCode::ProtocolError, err.to_string());
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
                self.fail(
                    ErrorCode::ProtocolError,
                    format!(
                        "{frame_type:?} on channel {channel} with {} bytes",
                        payload.len()
                    ),
                );
                return false;
            }
            if frame_type == FrameType::Hello {
                if self.hello_done {
                    self.fail(ErrorCode::ProtocolError, "second HELLO");
                    return false;
                }
                if !self.hello(&payload) {
                    return false;
                }
                continue;
            }
            if !self.hello_done {
                self.fail(ErrorCode::ProtocolError, "frame before HELLO");
                return false;
            }
            match frame_type {
                FrameType::Request => {
                    if !self.request(&payload).await {
                        return false;
                    }
                }
                FrameType::Open => self.send(
                    FrameType::Error,
                    channel,
                    &ErrorMessage {
                        id: None,
                        code: ErrorCode::Internal,
                        message: "attach is not implemented".into(),
                    },
                ),
                // Attachment frames on channels not in use are dropped.
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
                self.fail(ErrorCode::ProtocolError, "malformed HELLO");
                return false;
            }
        };
        if raw.get("version").and_then(|v| v.as_u64()) != Some(u64::from(proto::VERSION)) {
            self.fail(
                ErrorCode::VersionMismatch,
                format!("version {} is not {}", raw["version"], proto::VERSION),
            );
            return false;
        }
        if serde_json::from_value::<Hello>(raw).is_err() {
            self.fail(ErrorCode::ProtocolError, "malformed HELLO");
            return false;
        }
        self.hello_done = true;
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
        match control::handle(&self.registry, &mut self.conn, request).await {
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
}
