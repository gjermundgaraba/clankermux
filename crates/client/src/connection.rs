//! A client connection: framing over any byte stream, requests answered
//! by id, events, and frames routed to attachment channels.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use proto::{
    ErrorMessage, Event, FrameDecoder, FrameType, Header, Hello, Limits, Open, Peer, Welcome,
    encode_frame,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Notify, mpsc, oneshot};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The server broke the protocol or the connection ended unexpectedly.
    Protocol(String),
    /// A stream frame did not continue from the cursor: the attachment
    /// lost its place and a fresh `OPEN` recovers it.
    Desync(String),
    /// A server error reply.
    Server(ErrorMessage),
    /// The connection is closed.
    Closed,
    /// A caller's mistake, reported as given.
    Usage(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Desync(m) => write!(f, "out of sync: {m}"),
            Error::Server(e) => write!(f, "{}: {}", e.code, e.message),
            Error::Closed => write!(f, "connection closed"),
            Error::Usage(m) => write!(f, "{m}"),
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

#[derive(Default)]
struct Routes {
    pending: HashMap<i64, oneshot::Sender<Result<Value, ErrorMessage>>>,
    channels: HashMap<u32, mpsc::UnboundedSender<Frame>>,
    events: Option<mpsc::UnboundedSender<Event>>,
    /// Set once the reader ended.
    closed: Option<Ended>,
}

/// Why a connection ended.
#[derive(Debug, Clone)]
enum Ended {
    /// The server sent an `ERROR` without id.
    Server(ErrorMessage),
    /// The stream ended or broke.
    Lost(String),
    /// The server closed the connection, as on a stop.
    Eof,
}

impl From<&Ended> for Error {
    fn from(ended: &Ended) -> Self {
        match ended {
            Ended::Server(error) => Error::Server(error.clone()),
            Ended::Lost(reason) => Error::Protocol(reason.clone()),
            Ended::Eof => Error::Closed,
        }
    }
}

pub struct Connection {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    pub welcome: Welcome,
    next_id: AtomicI64,
    routes: Arc<Mutex<Routes>>,
    ended: Arc<Notify>,
}

/// The `HELLO` a client of `name` sends; `engine` is set for attaching
/// clients.
pub fn hello(name: &str, engine: Option<&str>) -> Hello {
    Hello {
        version: proto::VERSION,
        client: Peer {
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
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        // A flush per frame: a message carrier (the WebSocket adapter)
        // sends nothing before one.
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });
        let _ = tx.send(encode_frame(
            FrameType::Hello,
            0,
            &serde_json::to_vec(&hello).expect("serializable"),
        ));
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
                0 => return Err(Error::Protocol("closed before WELCOME".into())),
                n => decoder.push(&buf[..n]),
            }
        };
        let routes = Arc::new(Mutex::new(Routes::default()));
        let ended = Arc::new(Notify::new());
        let max_frame = welcome.limits.max_frame;
        tokio::spawn(read_loop(
            reader,
            decoder,
            max_frame,
            routes.clone(),
            ended.clone(),
        ));
        Ok(Connection {
            tx,
            welcome,
            next_id: AtomicI64::new(1),
            routes,
            ended,
        })
    }

    pub fn send(&self, frame_type: FrameType, channel: u32, payload: &[u8]) -> Result<(), Error> {
        self.tx
            .send(encode_frame(frame_type, channel, payload))
            .map_err(|_| self.ended().unwrap_or(Error::Closed))
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
        match reply_rx.await {
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
    /// receiver ends when the connection does.
    pub fn open(&self, channel: u32, open: &Open) -> Result<mpsc::UnboundedReceiver<Frame>, Error> {
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut routes = self.routes.lock().unwrap();
            if let Some(closed) = &routes.closed {
                return Err(closed.into());
            }
            routes.channels.insert(channel, tx);
        }
        self.send_json(FrameType::Open, channel, open)?;
        Ok(rx)
    }

    /// The sink for `EVENT` frames; `subscribe` must be requested too.
    /// The receiver ends when the connection does.
    pub fn events(&self) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut routes = self.routes.lock().unwrap();
        if routes.closed.is_none() {
            routes.events = Some(tx);
        }
        rx
    }

    /// Why the connection ended, once it has.
    pub fn ended(&self) -> Option<Error> {
        self.routes.lock().unwrap().closed.as_ref().map(Error::from)
    }

    /// Resolves once the connection has ended, with the reason.
    pub async fn closed(&self) -> Error {
        loop {
            let notified = self.ended.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(error) = self.ended() {
                return error;
            }
            notified.await;
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    mut decoder: FrameDecoder,
    max_frame: u64,
    routes: Arc<Mutex<Routes>>,
    ended: Arc<Notify>,
) {
    let mut buf = vec![0u8; 64 * 1024];
    let reason = loop {
        match decoder.next_frame(max_frame) {
            Ok(Some((header, payload))) => {
                if let Err(reason) = route(&routes, Frame { header, payload }) {
                    break reason;
                }
                continue;
            }
            Ok(None) => {}
            Err(err) => break Ended::Lost(err.to_string()),
        }
        match reader.read(&mut buf).await {
            Ok(0) => break Ended::Eof,
            Ok(n) => decoder.push(&buf[..n]),
            Err(err) => break Ended::Lost(err.to_string()),
        }
    };
    let mut routes = routes.lock().unwrap();
    routes.closed = Some(reason);
    routes.pending.clear();
    routes.channels.clear();
    routes.events = None;
    ended.notify_waiters();
}

/// Delivers one frame; an error ends the connection.
fn route(routes: &Mutex<Routes>, frame: Frame) -> Result<(), Ended> {
    let lost = |e: Error| Ended::Lost(e.to_string());
    let mut routes = routes.lock().unwrap();
    let channel = frame.header.channel;
    if channel != 0 {
        if let Some(tx) = routes.channels.get(&channel)
            && tx.send(frame).is_err()
        {
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
            let event: Event = frame.json().map_err(lost)?;
            if let Some(tx) = &routes.events
                && tx.send(event).is_err()
            {
                routes.events = None;
            }
        }
        other => return Err(Ended::Lost(format!("{other:?} on channel 0"))),
    }
    Ok(())
}

/// Where a server is reached: the local socket, a tailnet address, or a
/// relay over SSH.
#[derive(Debug, Clone, PartialEq, Eq)]
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
                    while let Ok(n @ 1..) =
                        tokio::io::AsyncReadExt::read(&mut stderr, &mut buf).await
                    {
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

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Unix(path) => write!(f, "{}", path.display()),
            Endpoint::Tcp(addr) => write!(f, "{addr}"),
            Endpoint::Ssh(destination) => write!(f, "ssh {destination}"),
        }
    }
}
