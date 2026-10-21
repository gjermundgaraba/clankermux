//! A client connection: framing over any byte stream, requests answered
//! by id, events, and frames routed to attachment channels.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use proto::{
    ClientPeer, ErrorMessage, FrameDecoder, FrameType, Header, Hello, Limits, Numbered, Open,
    Welcome, encode_frame, encode_json_frame,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};

pub use crate::queue::Receiver;
use crate::queue::{self, SendError, Sender, WeakSender};

const ATTACHMENT_QUEUE_BYTES: usize = 4 * 1024 * 1024;
const EVENT_QUEUE_BYTES: usize = 1024 * 1024;
const OUTGOING_QUEUE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The server broke the protocol.
    Protocol(String),
    /// A stream frame did not continue from the cursor: the attachment
    /// lost its place and a fresh `OPEN` recovers it.
    Desync(String),
    /// A server error reply.
    Server(ErrorMessage),
    /// The connection is closed.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Desync(m) => write!(f, "out of sync: {m}"),
            Error::Server(e) => write!(f, "{}: {}", e.code, e.message),
            Error::Closed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<ErrorMessage> for Error {
    fn from(e: ErrorMessage) -> Self {
        Error::Server(e)
    }
}

/// One received frame.
#[derive(Debug)]
pub struct Frame {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, Error> {
        serde_json::from_slice(&self.payload)
            .map_err(|e| Error::Protocol(format!("{:?} payload: {e}", self.header.frame_type)))
    }
}

enum Channel {
    Open(Sender<Frame>),
    /// Discard the old stream until the server acknowledges its end.
    Closing,
}

struct Routes {
    pending: HashMap<i64, oneshot::Sender<Result<Value, ErrorMessage>>>,
    channels: HashMap<u32, Channel>,
    events: Option<Sender<Numbered>>,
    stop: watch::Sender<bool>,
    /// Set before receivers close, including local queue overflow.
    closed: Option<Ended>,
}

/// Why a connection ended.
#[derive(Debug)]
enum Ended {
    /// A local queue overflowed; stream state must not be resumed.
    Desync(String),
    /// The server sent an `ERROR` without id.
    Server(ErrorMessage),
    /// The server broke the protocol.
    Protocol(String),
    /// The stream broke.
    Io(io::ErrorKind, String),
    /// The server closed the connection, as on a stop.
    Eof,
}

impl From<&Ended> for Error {
    fn from(ended: &Ended) -> Self {
        match ended {
            Ended::Desync(reason) => Error::Desync(reason.clone()),
            Ended::Server(error) => Error::Server(error.clone()),
            Ended::Protocol(reason) => Error::Protocol(reason.clone()),
            Ended::Io(kind, reason) => Error::Io(io::Error::new(*kind, reason.clone())),
            Ended::Eof => Error::Closed,
        }
    }
}

/// The reply to a request in flight.
pub struct Reply(oneshot::Receiver<Result<Value, ErrorMessage>>);

pub struct Connection {
    tx: Sender<Vec<u8>>,
    pub welcome: Welcome,
    next_id: AtomicI64,
    routes: Arc<Mutex<Routes>>,
}

/// The `HELLO` a client of `name` sends; `engine` is set for attaching
/// clients.
pub fn hello(name: &str, engine: Option<&str>) -> Hello {
    Hello {
        version: proto::VERSION,
        client: ClientPeer {
            name: name.into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        engine: engine.map(str::to_string),
        capabilities: Vec::new(),
    }
}

impl Connection {
    pub async fn connect_unix(path: &std::path::Path, hello: Hello) -> Result<Self, Error> {
        let stream = UnixStream::connect(path).await?;
        Self::handshake(stream, hello).await
    }

    /// Sends `HELLO`, waits for `WELCOME`, then routes frames in the
    /// background until the stream ends.
    pub async fn handshake<S: AsyncRead + AsyncWrite + Send + 'static>(
        stream: S,
        hello: Hello,
    ) -> Result<Self, Error> {
        let (mut reader, mut writer) = tokio::io::split(stream);
        writer
            .write_all(&encode_json_frame(FrameType::Hello, 0, &hello))
            .await?;
        writer.flush().await?;
        let mut decoder = FrameDecoder::default();
        let mut buf = vec![0u8; 64 * 1024];
        let welcome = loop {
            if let Some((header, payload)) = decoder
                .next_frame(Limits::default().max_frame)
                .map_err(|e| Error::Protocol(e.to_string()))?
            {
                let frame = Frame { header, payload };
                match header.frame_type {
                    FrameType::Welcome => break frame.json::<Welcome>()?,
                    FrameType::Error => return Err(frame.json::<ErrorMessage>()?.into()),
                    other => return Err(Error::Protocol(format!("{other:?} before WELCOME"))),
                }
            }
            match reader.read(&mut buf).await? {
                0 => return Err(Error::Closed),
                n => decoder.push(&buf[..n]),
            }
        };
        let (tx, rx) = queue::channel(OUTGOING_QUEUE_BYTES);
        let (stop, stopped) = watch::channel(false);
        let routes = Arc::new(Mutex::new(Routes {
            pending: HashMap::new(),
            channels: HashMap::new(),
            events: None,
            closed: None,
            stop,
        }));
        let max_frame = welcome.limits.max_frame;
        tokio::spawn(write_loop(writer, rx, routes.clone(), stopped.clone()));
        tokio::spawn(read_loop(
            reader,
            decoder,
            max_frame,
            routes.clone(),
            tx.downgrade(),
            stopped,
        ));
        Ok(Connection {
            tx,
            welcome,
            next_id: AtomicI64::new(1),
            routes,
        })
    }

    pub fn send(&self, frame_type: FrameType, channel: u32, payload: &[u8]) -> Result<(), Error> {
        if let Some(error) = self.ended() {
            return Err(error);
        }
        match self.tx.send(
            encode_frame(frame_type, channel, payload),
            proto::HEADER_LEN + payload.len(),
        ) {
            Ok(()) => Ok(()),
            Err(SendError::Full) => {
                end_routes(
                    &self.routes,
                    Ended::Desync("outgoing queue full; reconnect with fresh OPENs".into()),
                );
                Err(self.ended().expect("just ended"))
            }
            Err(SendError::Closed) => Err(self.ended().unwrap_or(Error::Closed)),
        }
    }

    pub fn send_json<T: Serialize>(
        &self,
        frame_type: FrameType,
        channel: u32,
        body: &T,
    ) -> Result<(), Error> {
        self.send(
            frame_type,
            channel,
            &serde_json::to_vec(body).expect("serializable"),
        )
    }

    /// A request and its reply.
    pub async fn request<T: Serialize>(&self, method: &str, params: T) -> Result<Value, Error> {
        let (_, reply) = self.begin(method, params)?;
        self.finish(reply).await
    }

    /// Sends a request; its id, and the reply to pass to `finish`. The id
    /// is what a `cancel` names.
    pub fn begin<T: Serialize>(&self, method: &str, params: T) -> Result<(i64, Reply), Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut routes = self.routes.lock().unwrap();
            if let Some(closed) = &routes.closed {
                return Err(closed.into());
            }
            routes.pending.insert(id, reply_tx);
        }
        self.send_json(
            FrameType::Request,
            0,
            &proto::Request {
                id,
                method: method.into(),
                params: serde_json::to_value(params).expect("serializable"),
            },
        )?;
        Ok((id, Reply(reply_rx)))
    }

    /// The reply to a request `begin` sent.
    pub async fn finish(&self, reply: Reply) -> Result<Value, Error> {
        match reply.0.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error.into()),
            // The connection ended, and says why (an id-less ERROR among
            // the reasons).
            Err(_) => Err(self.ended().unwrap_or(Error::Closed)),
        }
    }

    /// A typed request.
    pub async fn call<T: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, Error> {
        let value = self.request(method, params).await?;
        serde_json::from_value(value).map_err(|e| Error::Protocol(format!("{method} reply: {e}")))
    }

    /// Opens `channel` and returns the frames the server sends on it. The
    /// receiver ends after the attachment or connection ends. A local
    /// overflow delivers `ERROR too_slow`; use a fresh OPEN on a new channel
    /// or connection. The old channel stays reserved until its server end.
    pub fn open(&self, channel: u32, open: &Open) -> Result<Receiver<Frame>, Error> {
        let (tx, rx) = queue::channel(ATTACHMENT_QUEUE_BYTES);
        {
            let mut routes = self.routes.lock().unwrap();
            if let Some(closed) = &routes.closed {
                return Err(closed.into());
            }
            if routes.channels.contains_key(&channel) {
                return Err(Error::Protocol("attachment channel is still open".into()));
            }
            routes.channels.insert(channel, Channel::Open(tx));
        }
        self.send_json(FrameType::Open, channel, open)?;
        Ok(rx)
    }

    /// The sink for `EVENT` frames; `subscribe` must be requested too.
    /// The receiver ends when the connection does.
    pub fn events(&self) -> Receiver<Numbered> {
        let (tx, rx) = queue::channel(EVENT_QUEUE_BYTES);
        let mut routes = self.routes.lock().unwrap();
        if routes.closed.is_none() {
            routes.events = Some(tx);
        }
        rx
    }

    /// Why the connection ended, once it has: by the time `events()`,
    /// a reply or a channel's frames end.
    pub fn ended(&self) -> Option<Error> {
        self.routes.lock().unwrap().closed.as_ref().map(Error::from)
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    mut decoder: FrameDecoder,
    max_frame: u64,
    routes: Arc<Mutex<Routes>>,
    outgoing: WeakSender<Vec<u8>>,
    mut stopped: watch::Receiver<bool>,
) {
    let mut buf = vec![0u8; 64 * 1024];
    let reason = loop {
        if *stopped.borrow() {
            return;
        }
        match decoder.next_frame(max_frame) {
            Ok(Some((header, payload))) => {
                if let Err(reason) = route(&routes, &outgoing, Frame { header, payload }) {
                    break reason;
                }
                continue;
            }
            Ok(None) => {}
            Err(err) => break Ended::Protocol(err.to_string()),
        }
        let read = tokio::select! {
            biased;
            _ = stopped.changed() => return,
            read = reader.read(&mut buf) => read,
        };
        match read {
            Ok(0) => break Ended::Eof,
            Ok(n) => decoder.push(&buf[..n]),
            Err(err) => break Ended::Io(err.kind(), err.to_string()),
        }
    };
    end_routes(&routes, reason);
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: Receiver<Vec<u8>>,
    routes: Arc<Mutex<Routes>>,
    mut stopped: watch::Receiver<bool>,
) {
    let reason = loop {
        let next = tokio::select! {
            biased;
            _ = stopped.changed() => return,
            next = rx.rx.recv() => next,
        };
        let Some(bytes) = next else {
            break Ended::Eof;
        };
        // Keep the byte permit until the carrier has flushed this frame.
        let result = tokio::select! {
            biased;
            _ = stopped.changed() => return,
            result = async {
                writer.write_all(&bytes.value).await?;
                writer.flush().await
            } => result,
        };
        if let Err(err) = result {
            break Ended::Io(err.kind(), err.to_string());
        }
    };
    end_routes(&routes, reason);
    let _ = writer.shutdown().await;
}

fn loss_frame(channel: u32, message: &str) -> Frame {
    let payload = serde_json::to_vec(&ErrorMessage {
        id: None,
        code: proto::ErrorCode::TooSlow,
        message: message.into(),
    })
    .expect("serializable");
    Frame {
        header: Header {
            frame_type: FrameType::Error,
            channel,
            length: payload.len() as u32,
        },
        payload,
    }
}

fn end_routes(routes: &Mutex<Routes>, reason: Ended) {
    let mut routes = routes.lock().unwrap();
    if routes.closed.is_some() {
        return;
    }
    for (channel, state) in routes.channels.drain() {
        if let (Channel::Open(tx), Ended::Desync(message)) = (state, &reason) {
            tx.end(loss_frame(channel, message));
        }
    }
    routes.closed = Some(reason);
    routes.pending.clear();
    routes.events = None;
    routes.stop.send_replace(true);
}

/// Delivers one frame; an error ends the connection.
fn route(
    routes: &Mutex<Routes>,
    outgoing: &WeakSender<Vec<u8>>,
    frame: Frame,
) -> Result<(), Ended> {
    let lost = |e: Error| Ended::Protocol(e.to_string());
    let mut routes = routes.lock().unwrap();
    let channel = frame.header.channel;
    if routes.closed.is_some() {
        return Ok(());
    }
    if channel != 0 {
        let Some(state) = routes.channels.get(&channel) else {
            return Ok(());
        };
        let terminal = frame.header.frame_type == FrameType::Closed
            || (frame.header.frame_type == FrameType::Error
                && !crate::attachment::keeps_attachment(
                    frame.json::<ErrorMessage>().map_err(lost)?.code,
                ));
        if let Channel::Open(tx) = state {
            let bytes = proto::HEADER_LEN + frame.payload.len();
            if let Err(error) = tx.send(frame, bytes) {
                if let Some(Channel::Open(tx)) = routes.channels.insert(channel, Channel::Closing)
                    && error == SendError::Full
                {
                    tx.end(loss_frame(
                        channel,
                        "attachment queue full; fresh OPEN required",
                    ));
                }
                if !terminal {
                    let close = encode_frame(FrameType::Close, channel, &[]);
                    outgoing.send(close, proto::HEADER_LEN).map_err(|_| {
                        Ended::Desync(
                            "cannot queue CLOSE after local loss; reconnect with fresh OPENs"
                                .into(),
                        )
                    })?;
                }
            }
        }
        if terminal {
            routes.channels.remove(&channel);
        }
        return Ok(());
    }
    match frame.header.frame_type {
        FrameType::Reply => {
            let reply: proto::Reply = frame.json().map_err(lost)?;
            // A reply or error for a request no longer waited for is dropped.
            if let Some(tx) = routes.pending.remove(&reply.id) {
                let _ = tx.send(Ok(reply.result));
            }
        }
        FrameType::Error => {
            let error: ErrorMessage = frame.json().map_err(lost)?;
            match error.id {
                Some(id) => {
                    if let Some(tx) = routes.pending.remove(&id) {
                        let _ = tx.send(Err(error));
                    }
                }
                // An error without id precedes the server closing.
                None => return Err(Ended::Server(error)),
            }
        }
        FrameType::Event => {
            let event: Numbered = frame.json().map_err(lost)?;
            if let Some(tx) = &routes.events {
                match tx.send(event, proto::HEADER_LEN + frame.payload.len()) {
                    Ok(()) => {}
                    Err(SendError::Closed) => routes.events = None,
                    Err(SendError::Full) => {
                        return Err(Ended::Desync(
                            "event queue full; reconnect and resubscribe, with fresh OPENs".into(),
                        ));
                    }
                }
            }
        }
        other => return Err(Ended::Protocol(format!("{other:?} on channel 0"))),
    }
    Ok(())
}

/// Where a server is reached: the local socket, a tailnet address, or a
/// relay over SSH.
#[derive(Debug, Clone)]
pub enum Endpoint {
    Unix(std::path::PathBuf),
    /// `host:port`, plaintext.
    Tcp(String),
    /// An `ssh` destination running `clankermux --stdio`.
    Ssh(String),
}

impl Endpoint {
    pub async fn connect(&self, hello: Hello) -> Result<Connection, Error> {
        match self {
            Endpoint::Unix(path) => Connection::connect_unix(path, hello).await,
            Endpoint::Tcp(addr) => {
                let stream = tokio::net::TcpStream::connect(addr.as_str()).await?;
                let _ = stream.set_nodelay(true);
                Connection::handshake(stream, hello).await
            }
            Endpoint::Ssh(destination) => {
                // The child outlives this handle: it ends when the pipes
                // close, and tokio reaps it. Its stderr would otherwise
                // print into an attached terminal in raw mode; its last
                // bytes are kept for the error when the handshake fails,
                // and discarded for as long as the connection lives.
                let mut child = tokio::process::Command::new("ssh")
                    .arg(destination)
                    .args(["clankermux", "--stdio"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()?;
                let stdin = child.stdin.take().expect("piped");
                let stdout = child.stdout.take().expect("piped");
                let mut stderr = child.stderr.take().expect("piped");
                let drain = tokio::spawn(async move {
                    const KEPT: usize = 4096;
                    let mut tail = Vec::new();
                    let mut buf = [0u8; 1024];
                    while let Ok(n @ 1..) = stderr.read(&mut buf).await {
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > KEPT {
                            tail.drain(..tail.len() - KEPT);
                        }
                    }
                    String::from_utf8_lossy(&tail).into_owned()
                });
                match Connection::handshake(tokio::io::join(stdout, stdin), hello).await {
                    Ok(connection) => Ok(connection),
                    Err(error) => {
                        drop(child);
                        // What ssh said explains a transport failure; a
                        // server's refusal stands on its own.
                        if !matches!(error, Error::Io(_) | Error::Closed) {
                            return Err(error);
                        }
                        let said = tokio::time::timeout(std::time::Duration::from_secs(2), drain)
                            .await
                            .ok()
                            .and_then(|r| r.ok())
                            .unwrap_or_default();
                        match said.trim() {
                            "" => Err(error),
                            said => {
                                Err(Error::Io(io::Error::other(format!("{error}; ssh: {said}"))))
                            }
                        }
                    }
                }
            }
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Unix(path) => write!(f, "{}", path.display()),
            Endpoint::Tcp(addr) => write!(f, "{addr}"),
            Endpoint::Ssh(destination) => write!(f, "ssh {destination}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn routes() -> Mutex<Routes> {
        let (stop, _) = watch::channel(false);
        Mutex::new(Routes {
            pending: HashMap::new(),
            channels: HashMap::new(),
            events: None,
            closed: None,
            stop,
        })
    }

    fn frame(kind: FrameType, channel: u32, payload: Vec<u8>) -> Frame {
        Frame {
            header: Header {
                frame_type: kind,
                channel,
                length: payload.len() as u32,
            },
            payload,
        }
    }

    #[tokio::test]
    async fn attachment_overflow_signals_loss_without_blocking_other_routes() {
        let routes = routes();
        let (outgoing, mut writes) = queue::channel(1024);
        let out = outgoing.downgrade();
        let charge = size_of::<queue::Queued<Frame>>() + proto::HEADER_LEN + 9;
        let (slow, mut slow_rx) = queue::channel(charge);
        let (fast, mut fast_rx) = queue::channel(1024);
        let (reply, mut reply_rx) = oneshot::channel();
        {
            let mut r = routes.lock().unwrap();
            r.channels.insert(1, Channel::Open(slow));
            r.channels.insert(2, Channel::Open(fast));
            r.pending.insert(7, reply);
        }
        for offset in 0..3 {
            route(
                &routes,
                &out,
                frame(FrameType::Output, 1, proto::encode_output(offset, b"x")),
            )
            .unwrap();
        }
        assert_eq!(
            slow_rx.recv().await.unwrap().header.frame_type,
            FrameType::Output
        );
        let loss = slow_rx
            .recv()
            .await
            .unwrap()
            .json::<ErrorMessage>()
            .unwrap();
        assert_eq!(loss.code, proto::ErrorCode::TooSlow);
        assert!(slow_rx.recv().await.is_none());
        let close = writes.try_recv().unwrap();
        assert_eq!(
            proto::parse_frame(&close, 1024).unwrap().0.frame_type,
            FrameType::Close
        );
        assert!(writes.is_empty(), "only one CLOSE for the lost stream");
        assert!(matches!(
            routes.lock().unwrap().channels.get(&1),
            Some(Channel::Closing)
        ));

        route(
            &routes,
            &out,
            frame(FrameType::Output, 2, proto::encode_output(0, b"y")),
        )
        .unwrap();
        route(
            &routes,
            &out,
            frame(
                FrameType::Reply,
                0,
                br#"{"id":7,"result":{"ok":true}}"#.to_vec(),
            ),
        )
        .unwrap();
        assert_eq!(
            proto::parse_output(&fast_rx.recv().await.unwrap().payload)
                .unwrap()
                .1,
            b"y"
        );
        assert_eq!(
            reply_rx.try_recv().unwrap().unwrap(),
            serde_json::json!({"ok": true})
        );
        assert!(routes.lock().unwrap().closed.is_none());
        route(
            &routes,
            &out,
            frame(FrameType::Closed, 1, br#"{"reason":"closed"}"#.to_vec()),
        )
        .unwrap();
        assert!(!routes.lock().unwrap().channels.contains_key(&1));
    }

    #[tokio::test]
    async fn event_overflow_ends_with_loss_and_invalidates_attachment_cursors() {
        let routes = routes();
        let (outgoing, _) = queue::channel(1024);
        let event = Numbered {
            seq: 1,
            event: proto::Event::Bell { id: "s".into() },
        };
        let payload = serde_json::to_vec(&event).unwrap();
        let (events, mut events_rx) = queue::channel(
            size_of::<queue::Queued<Numbered>>() + proto::HEADER_LEN + payload.len(),
        );
        let (frames, mut frames_rx) = queue::channel(1024);
        {
            let mut r = routes.lock().unwrap();
            r.events = Some(events);
            r.channels.insert(1, Channel::Open(frames));
        }
        route(
            &routes,
            &outgoing.downgrade(),
            frame(FrameType::Event, 0, payload.clone()),
        )
        .unwrap();
        let reason = route(
            &routes,
            &outgoing.downgrade(),
            frame(FrameType::Event, 0, payload),
        )
        .unwrap_err();
        assert!(matches!(reason, Ended::Desync(_)));
        end_routes(&routes, reason);
        assert_eq!(events_rx.recv().await.unwrap().seq, 1);
        assert!(events_rx.recv().await.is_none());
        assert!(matches!(
            routes.lock().unwrap().closed,
            Some(Ended::Desync(_))
        ));
        assert_eq!(
            frames_rx
                .recv()
                .await
                .unwrap()
                .json::<ErrorMessage>()
                .unwrap()
                .code,
            proto::ErrorCode::TooSlow
        );
        assert!(frames_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn outgoing_overflow_cancels_a_blocked_writer_and_pending_replies() {
        let (socket, mut peer) = tokio::io::duplex(64);
        let handshake = tokio::spawn(async move {
            let mut decoder = FrameDecoder::default();
            let mut buf = [0; 64];
            loop {
                let n = peer.read(&mut buf).await.unwrap();
                assert!(n > 0);
                decoder.push(&buf[..n]);
                if let Some((header, _)) = decoder.next_frame(1024).unwrap() {
                    assert_eq!(header.frame_type, FrameType::Hello);
                    break;
                }
            }
            let welcome = Welcome {
                version: proto::VERSION,
                server: proto::Peer {
                    name: "test".into(),
                    version: "0".into(),
                },
                engine: ghostty_vt::ENGINE.into(),
                incarnation: "test".into(),
                capabilities: vec![],
                limits: Limits::default(),
            };
            peer.write_all(&encode_json_frame(FrameType::Welcome, 0, &welcome))
                .await
                .unwrap();
            peer
        });
        let conn = Connection::handshake(socket, hello("test", Some(ghostty_vt::ENGINE)))
            .await
            .unwrap();
        let mut peer = handshake.await.unwrap();
        let mut frames = conn
            .open(
                1,
                &Open {
                    session: "s".into(),
                    read_only: false,
                    viewport: None,
                    profile: None,
                    resume: None,
                },
            )
            .unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        conn.send(FrameType::Input, 1, &chunk).unwrap();
        tokio::task::yield_now().await;
        let (_, reply) = conn.begin("list", serde_json::json!({})).unwrap();
        let mut sent = 1;
        loop {
            match conn.send(FrameType::Input, 1, &chunk) {
                Ok(()) => sent += 1,
                Err(Error::Desync(_)) => break,
                other => panic!("unexpected send result: {other:?}"),
            }
            assert!(sent <= OUTGOING_QUEUE_BYTES / chunk.len());
        }
        assert!(matches!(conn.ended(), Some(Error::Desync(_))));
        assert!(matches!(conn.finish(reply).await, Err(Error::Desync(_))));
        assert_eq!(
            frames
                .recv()
                .await
                .unwrap()
                .json::<ErrorMessage>()
                .unwrap()
                .code,
            proto::ErrorCode::TooSlow
        );
        assert!(frames.recv().await.is_none());
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), peer.read_to_end(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert!(received.len() <= 64, "the blocked writer was cancelled");
    }
}
