//! The TCP and TLS WebSocket carriers.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use proto::*;
use serde_json::json;
use testkit::{ClientWs, connect_ws};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

/// A session, an attach with its bootstrap, and a close, over `c`.
async fn full_attach(c: &mut Client) {
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "echo over-the-wire; exec cat"]}))
        .await;
    wait_text(c, &session.id, "over-the-wire").await;
    c.open(
        1,
        json!({"session": session.id, "viewport": {"cols": 40, "rows": 8}}),
    )
    .await;
    let boot = c.bootstrap(1).await;
    assert!(boot.text().contains("over-the-wire"), "{:?}", boot.text());
    c.send(FrameType::Close, 1, &[]).await;
    assert_eq!(c.expect_closed(1).await, ClosedReason::Closed);
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_carries_frames() {
    let server = start(fast_limits());
    let addr = server.listen_tcp().await;
    let mut c = Client::over(TcpStream::connect(addr).await.unwrap());
    c.hello().await;
    full_attach(&mut c).await;
}

/// A WebSocket carrier on loopback with a self-signed certificate the
/// client trusts.
struct Ws {
    addr: SocketAddr,
    connector: TlsConnector,
}

async fn serve_ws(server: &Server, origins: Vec<String>) -> Ws {
    let (addr, connector) = server.listen_wss(origins).await;
    Ws { addr, connector }
}

impl Ws {
    async fn connect(&self, origin: Option<&str>) -> Result<ClientWs, WsError> {
        connect_ws(self.addr, &self.connector, "/any/path", origin).await
    }
}

fn refused(result: Result<ClientWs, WsError>) -> StatusCode {
    match result {
        Err(WsError::Http(response)) => response.status(),
        Ok(_) => panic!("upgrade accepted"),
        Err(other) => panic!("{other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_websocket_admits_allowed_origins_only() {
    let server = start(fast_limits());
    let ws = serve_ws(&server, vec!["http://allowed".into()]).await;
    assert_eq!(refused(ws.connect(None).await), StatusCode::FORBIDDEN);
    assert_eq!(
        refused(ws.connect(Some("http://other")).await),
        StatusCode::FORBIDDEN
    );
    let accepted = ws.connect(Some("http://allowed")).await.unwrap();
    let mut c = Client::over(server::ws::WsStream::new(
        accepted,
        Limits::default().max_frame,
    ));
    c.hello().await;
    full_attach(&mut c).await;

    // No allowlist admits nobody.
    let closed = serve_ws(&server, Vec::new()).await;
    assert_eq!(
        refused(closed.connect(Some("http://allowed")).await),
        StatusCode::FORBIDDEN
    );
}

/// The next message from the server, which must be one whole frame.
async fn next(raw: &mut ClientWs) -> (Header, Vec<u8>) {
    let max_frame = Limits::default().max_frame;
    let message = tokio::time::timeout(Duration::from_secs(5), raw.next())
        .await
        .expect("a message in time")
        .expect("a message")
        .unwrap();
    let bytes = match message {
        Message::Binary(bytes) => bytes,
        other => panic!("{other:?}"),
    };
    let (header, payload) = parse_frame(&bytes, max_frame).unwrap();
    assert_eq!(HEADER_LEN + header.length as usize, bytes.len());
    (header, payload.to_vec())
}

fn json(frame_type: FrameType, body: serde_json::Value) -> Message {
    Message::Binary(encode_frame(frame_type, 0, &serde_json::to_vec(&body).unwrap()).into())
}

/// A raw connection past its handshake.
async fn greeted(ws: &Ws) -> ClientWs {
    let mut raw = ws.connect(Some("http://allowed")).await.unwrap();
    raw.send(json(
        FrameType::Hello,
        json!({"version": 1, "client": {"name": "raw", "version": "0"}, "capabilities": [],
            "engine": ghostty_vt::ENGINE}),
    ))
    .await
    .unwrap();
    assert_eq!(next(&mut raw).await.0.frame_type, FrameType::Welcome);
    raw
}

/// Reads to the close: an `ERROR protocol_error` on channel 0 must come,
/// and no reply.
async fn ends_with_protocol_error(raw: &mut ClientWs) -> String {
    let max_frame = Limits::default().max_frame;
    let mut error = None;
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), raw.next())
            .await
            .expect("the close in time");
        match message {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
            Some(Ok(Message::Binary(bytes))) => {
                let (header, payload) = parse_frame(&bytes, max_frame).unwrap();
                // Whatever was still queued for channel 1 may arrive first.
                match header.frame_type {
                    FrameType::Reply => panic!("a reply to a bad message"),
                    FrameType::Error if header.channel == 0 => {
                        let message: ErrorMessage = serde_json::from_slice(payload).unwrap();
                        assert_eq!(message.code, ErrorCode::ProtocolError, "{message:?}");
                        error = Some(message.message);
                    }
                    _ => {}
                }
            }
            Some(Ok(_)) => {}
        }
    }
    error.expect("ERROR protocol_error before the close")
}

#[tokio::test(flavor = "multi_thread")]
async fn each_message_is_exactly_one_frame() {
    let server = start(fast_limits());
    let ws = serve_ws(&server, vec!["http://allowed".into()]).await;
    let mut raw = greeted(&ws).await;
    raw.send(json(
        FrameType::Request,
        json!({"id": 1, "method": "create", "params": {"argv": ["/bin/cat"]}}),
    ))
    .await
    .unwrap();
    let (header, payload) = next(&mut raw).await;
    assert_eq!(header.frame_type, FrameType::Reply);
    let reply: Reply = serde_json::from_slice(&payload).unwrap();
    let session: Session = serde_json::from_value(reply.result).unwrap();
    raw.send(Message::Binary(
        encode_frame(
            FrameType::Open,
            1,
            &serde_json::to_vec(
                &json!({"session": session.id, "viewport": {"cols": 20, "rows": 5}}),
            )
            .unwrap(),
        )
        .into(),
    ))
    .await
    .unwrap();
    let mut seen = Vec::new();
    loop {
        let (header, payload) = next(&mut raw).await;
        assert_ne!(
            header.frame_type,
            FrameType::Error,
            "{}",
            String::from_utf8_lossy(&payload)
        );
        seen.push(header.frame_type);
        if header.frame_type == FrameType::SnapshotEnd {
            break;
        }
    }
    assert_eq!(seen[0], FrameType::Attached);
    assert!(seen.contains(&FrameType::SnapshotReady));

    // Two frames in one message are a protocol error.
    let mut two = encode_frame(
        FrameType::Request,
        0,
        br#"{"id":2,"method":"list","params":{}}"#,
    );
    two.extend(encode_frame(
        FrameType::Request,
        0,
        br#"{"id":3,"method":"list","params":{}}"#,
    ));
    raw.send(Message::Binary(two.into())).await.unwrap();
    assert!(
        ends_with_protocol_error(&mut raw)
            .await
            .contains("exactly one frame")
    );

    // A text message, too.
    let mut raw = greeted(&ws).await;
    raw.send(Message::Text("hello".into())).await.unwrap();
    assert!(
        ends_with_protocol_error(&mut raw)
            .await
            .contains("binary messages")
    );

    // A message shorter than a header, or empty, is not a frame either:
    // two such messages must not add up to one.
    for short in [&b"\x02\x00\x00\x00\x00"[..], &b""[..]] {
        let mut raw = greeted(&ws).await;
        raw.send(Message::Binary(short.to_vec().into()))
            .await
            .unwrap();
        assert!(
            ends_with_protocol_error(&mut raw)
                .await
                .contains("exactly one frame"),
            "{short:?}"
        );
    }

    // A message over the ceiling, too.
    let mut raw = greeted(&ws).await;
    let over = vec![0u8; Limits::default().max_frame as usize + HEADER_LEN + 1];
    // The server refuses it from its header, before the bytes are all
    // written, so the send itself may fail.
    let _ = raw.send(Message::Binary(over.into())).await;
    assert!(
        ends_with_protocol_error(&mut raw)
            .await
            .contains("too long")
    );

    // A bad header is reported as on any carrier.
    let mut raw = greeted(&ws).await;
    let mut bad = encode_frame(FrameType::Request, 0, b"{}");
    bad[0] = 0x7f;
    raw.send(Message::Binary(bad.into())).await.unwrap();
    assert_eq!(
        ends_with_protocol_error(&mut raw).await,
        "unknown frame type 0x7f"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_connection_closes_within_hello_timeout() {
    let server = start(fast_limits());
    let ws = serve_ws(&server, vec!["http://allowed".into()]).await;
    let started = std::time::Instant::now();
    // Nothing sent: no TLS handshake, no upgrade.
    let mut tcp = TcpStream::connect(ws.addr).await.unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), tcp.read(&mut byte))
        .await
        .expect("the server closes");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(2),
        "{elapsed:?}"
    );
}
