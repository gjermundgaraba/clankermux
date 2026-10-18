//! The WebSocket carrier: TLS, the `Origin` allowlist, one frame per
//! binary message.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use futures_util::{Sink, Stream};
use proto::{HEADER_LEN, parse_header};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Bytes, Message};

use crate::Registry;
use crate::connection::serve_connection;
use crate::tls::Tls;

/// Serves `listener` until cancelled. Every connection is TLS; the
/// upgrade is refused with 403 unless its `Origin` is in `origins`, so an
/// empty allowlist refuses them all.
pub async fn serve_ws(
    listener: TcpListener,
    registry: Arc<Registry>,
    tls: Arc<Tls>,
    origins: Vec<String>,
) -> io::Result<()> {
    let origins = Arc::new(origins);
    let max_frame = registry.config.limits.max_frame;
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let acceptor = tls.acceptor();
        let registry = registry.clone();
        let origins = origins.clone();
        tokio::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::debug!(%error, "TLS handshake failed");
                    return;
                }
            };
            // The callback's error type is tungstenite's HTTP response.
            #[allow(clippy::result_large_err)]
            let check = |request: &Request,
                         response: Response|
             -> Result<Response, ErrorResponse> {
                let origin = request
                    .headers()
                    .get("origin")
                    .and_then(|v| v.to_str().ok());
                match origin {
                    Some(origin) if origins.iter().any(|allowed| allowed == origin) => Ok(response),
                    _ => {
                        let mut refused = ErrorResponse::new(Some("origin not allowed".into()));
                        *refused.status_mut() = StatusCode::FORBIDDEN;
                        Err(refused)
                    }
                }
            };
            let config = WebSocketConfig::default()
                .max_message_size(Some(message_ceiling(max_frame)))
                .max_frame_size(Some(message_ceiling(max_frame)));
            let ws =
                match tokio_tungstenite::accept_hdr_async_with_config(stream, check, Some(config))
                    .await
                {
                    Ok(ws) => ws,
                    Err(error) => {
                        tracing::debug!(%error, "WebSocket upgrade failed");
                        return;
                    }
                };
            serve_connection(WsStream::new(ws, max_frame), registry).await;
        });
    }
}

/// The longest message that can hold one frame.
fn message_ceiling(max_frame: u64) -> usize {
    usize::try_from(max_frame).map_or(usize::MAX, |n| n.saturating_add(HEADER_LEN))
}

/// Frames over a WebSocket as a byte stream: a message read must be exactly
/// one frame; bytes written are sent as one message per frame once they
/// complete one. The writer flushes after every frame.
pub struct WsStream<S> {
    ws: WebSocketStream<S>,
    max_frame: u64,
    /// The last message, read from `pos`.
    incoming: Bytes,
    pos: usize,
    /// Written bytes short of a whole frame.
    outgoing: Vec<u8>,
}

impl<S> WsStream<S> {
    pub fn new(ws: WebSocketStream<S>, max_frame: u64) -> Self {
        Self {
            ws,
            max_frame,
            incoming: Bytes::new(),
            pos: 0,
            outgoing: Vec::new(),
        }
    }
}

/// The length of the frame `bytes` starts with, when its header is there.
fn frame_len(bytes: &[u8], max_frame: u64) -> io::Result<Option<usize>> {
    let Some(head) = bytes.get(..HEADER_LEN) else {
        return Ok(None);
    };
    let header = parse_header(head.try_into().unwrap(), max_frame)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some(HEADER_LEN + header.length as usize))
}

fn other(error: tokio_tungstenite::tungstenite::Error) -> io::Error {
    match error {
        tokio_tungstenite::tungstenite::Error::Io(e) => e,
        e => io::Error::other(e),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.pos < this.incoming.len() {
                let n = buf.remaining().min(this.incoming.len() - this.pos);
                buf.put_slice(&this.incoming[this.pos..this.pos + n]);
                this.pos += n;
                return Poll::Ready(Ok(()));
            }
            match ready!(Pin::new(&mut this.ws).poll_next(cx)) {
                Some(Ok(Message::Binary(bytes))) => {
                    if frame_len(&bytes, this.max_frame)? != Some(bytes.len()) {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "a message must hold exactly one frame",
                        )));
                    }
                    this.incoming = bytes;
                    this.pos = 0;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                    // The pong tungstenite queued leaves with this flush.
                    let _ = Pin::new(&mut this.ws).poll_flush(cx);
                }
                Some(Ok(Message::Close(_))) | None => return Poll::Ready(Ok(())),
                Some(Ok(Message::Text(_) | Message::Frame(_))) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "frames travel as binary messages",
                    )));
                }
                Some(Err(e)) => return Poll::Ready(Err(other(e))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(Pin::new(&mut this.ws).poll_ready(cx)).map_err(other)?;
        this.outgoing.extend_from_slice(buf);
        while let Some(len) = frame_len(&this.outgoing, this.max_frame)?
            && len <= this.outgoing.len()
        {
            let frame: Vec<u8> = this.outgoing.drain(..len).collect();
            Pin::new(&mut this.ws)
                .start_send(Message::Binary(frame.into()))
                .map_err(other)?;
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().ws)
            .poll_flush(cx)
            .map_err(other)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().ws)
            .poll_close(cx)
            .map_err(other)
    }
}
