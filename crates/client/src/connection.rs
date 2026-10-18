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
            Error::Server(e) => {
                let code = serde_json::to_value(e.code).expect("serializable");
                write!(f, "{}: {}", code.as_str().unwrap_or("error"), e.message)
            }
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
}

impl From<&Ended> for Error {
    fn from(ended: &Ended) -> Self {
        match ended {
            Ended::Server(error) => Error::Server(error.clone()),
            Ended::Lost(reason) => Error::Protocol(reason.clone()),
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
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if writer.write_all(&bytes).await.is_err() {
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
            .map_err(|_| Error::Closed)
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
            if routes.closed.is_some() {
                return Err(Error::Closed);
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
            Err(_) => Err(Error::Closed),
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
            if routes.closed.is_some() {
                return Err(Error::Closed);
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
            Ok(0) => break Ended::Lost("connection closed by the server".into()),
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
