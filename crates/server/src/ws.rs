//! The WebSocket carrier: TLS, the `Origin` allowlist, one frame per
//! binary message.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

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
/// empty allowlist refuses them all. The TLS handshake and the upgrade
/// count against `hello_timeout` like the bytes before `HELLO`.
pub async fn serve_ws(
    listener: TcpListener,
    registry: Arc<Registry>,
    tls: Arc<Tls>,
    origins: Vec<String>,
) -> io::Result<()> {
    let origins = Arc::new(origins);
    let max_frame = registry.config.limits.max_frame;
    let hello_timeout = Duration::from_millis(registry.config.limits.hello_timeout);
    let connections = registry.clone();
    crate::serve_tcp_with(listener, &connections, move |stream, accepted| {
        let acceptor = tls.acceptor();
        let registry = registry.clone();
        let origins = origins.clone();
        async move {
            let upgrade = async {
                let stream = acceptor.accept(stream).await?;
                // The callback's error type is tungstenite's HTTP response.
                #[allow(clippy::result_large_err)]
                let check =
                    |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                        let allowed = request
                            .headers()
                            .get("origin")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| origin(v).ok())
                            .is_some_and(|o| origins.contains(&o));
                        if allowed {
                            Ok(response)
                        } else {
                            let mut refused = ErrorResponse::new(Some("origin not allowed".into()));
                            *refused.status_mut() = StatusCode::FORBIDDEN;
                            Err(refused)
                        }
                    };
                let config = WebSocketConfig::default()
                    .max_message_size(Some(message_ceiling(max_frame)))
                    .max_frame_size(Some(message_ceiling(max_frame)));
                tokio_tungstenite::accept_hdr_async_with_config(stream, check, Some(config))
                    .await
                    .map_err(other)
            };
            match tokio::time::timeout_at(accepted + hello_timeout, upgrade).await {
                Ok(Ok(ws)) => {
                    serve_connection(WsStream::new(ws, max_frame), registry, accepted).await
                }
                Ok(Err(error)) => tracing::debug!(%error, "WebSocket handshake failed"),
                Err(_) => tracing::debug!("WebSocket handshake timed out"),
            }
        }
    })
    .await
}

/// An origin as browsers serialize it: lowercase scheme and host, no
/// default port, no path; or `null`. Anything else is an error.
pub fn origin(text: &str) -> Result<String, String> {
    if text == "null" {
        return Ok(text.into());
    }
    let bad = || format!("{text:?} is not an origin (scheme://host[:port])");
    let (scheme, rest) = text.split_once("://").ok_or_else(bad)?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
        || rest.is_empty()
        || rest.contains(['/', '?', '#', '@'])
    {
        return Err(bad());
    }
    let scheme = scheme.to_ascii_lowercase();
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) if !rest.ends_with(']') => {
            (host, Some(port.parse::<u16>().map_err(|_| bad())?))
        }
        _ => (rest, None),
    };
    let default = match scheme.as_str() {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    };
    let host = host.to_ascii_lowercase();
    Ok(match port {
        Some(port) if Some(port) != default => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    })
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

/// The length of the frame `bytes` starts with, when its header is there
/// and valid.
fn frame_len(bytes: &[u8], max_frame: u64) -> Option<usize> {
    let head = bytes.get(..HEADER_LEN)?;
    let header = parse_header(head.try_into().unwrap(), max_frame).ok()?;
    Some(HEADER_LEN + header.length as usize)
}

fn other(error: tokio_tungstenite::tungstenite::Error) -> io::Error {
    match error {
        tokio_tungstenite::tungstenite::Error::Io(e) => e,
        e => io::Error::other(e),
    }
}

/// A carrier-level protocol error: the connection reports it before
/// closing.
fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
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
                    // A bad header passes through: the frame decoder
                    // reports it like on any other carrier.
                    if bytes.len() < HEADER_LEN
                        || frame_len(&bytes, this.max_frame).is_some_and(|len| len != bytes.len())
                    {
                        return Poll::Ready(Err(invalid("a message must hold exactly one frame")));
                    }
                    this.incoming = bytes;
                    this.pos = 0;
                }
                // tungstenite answers a ping with the next read.
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => return Poll::Ready(Ok(())),
                Some(Ok(_)) => {
                    return Poll::Ready(Err(invalid("frames travel as binary messages")));
                }
                Some(Err(e)) => return Poll::Ready(Err(other(e))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> WsStream<S> {
    /// Sends the whole frames buffered so far, one message each, as far
    /// as the sink is ready.
    fn send_frames(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while let Some(len) = frame_len(&self.outgoing, self.max_frame)
            && len <= self.outgoing.len()
        {
            ready!(Pin::new(&mut self.ws).poll_ready(cx)).map_err(other)?;
            let frame: Vec<u8> = self.outgoing.drain(..len).collect();
            Pin::new(&mut self.ws)
                .start_send(Message::Binary(frame.into()))
                .map_err(other)?;
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.send_frames(cx))?;
        this.outgoing.extend_from_slice(buf);
        if let Poll::Ready(Err(e)) = this.send_frames(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.send_frames(cx))?;
        Pin::new(&mut this.ws).poll_flush(cx).map_err(other)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().ws)
            .poll_close(cx)
            .map_err(other)
    }
}

#[cfg(test)]
mod tests {
    use super::origin;

    #[test]
    fn origins_take_the_form_browsers_send() {
        for (given, expected) in [
            ("http://page.test", "http://page.test"),
            ("HTTP://Page.Test/", "http://page.test"),
            ("https://node.ts.net:443", "https://node.ts.net"),
            ("http://127.0.0.1:8787/", "http://127.0.0.1:8787"),
            ("http://[::1]:8787", "http://[::1]:8787"),
            ("http://[::1]", "http://[::1]"),
            ("null", "null"),
        ] {
            assert_eq!(origin(given).as_deref(), Ok(expected), "{given}");
        }
        for bad in [
            "page.test",
            "http://",
            "http://page.test/app",
            "http://page.test:port",
            "http://user@page.test",
        ] {
            assert!(origin(bad).is_err(), "{bad}");
        }
    }
}
