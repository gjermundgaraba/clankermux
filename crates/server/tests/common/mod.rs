//! Shared test harness: an in-process server on a temp Unix socket and a
//! framed client.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use proto::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub struct Server {
    pub socket: PathBuf,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn start(limits: Limits) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("sock");
    let registry = server::Registry::new(server::Config {
        limits,
        shell: vec!["/bin/sh".into()],
        home: dir.path().to_path_buf(),
        version: "test".into(),
    });
    let task = tokio::spawn(server::serve_unix(socket.clone(), registry));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Server {
        socket,
        _dir: dir,
        task,
    }
}

pub fn fast_limits() -> Limits {
    Limits {
        hello_timeout: 300,
        remove_grace: 200,
        page: 2,
        ..Limits::default()
    }
}

pub struct Client {
    pub stream: UnixStream,
    pub decoder: FrameDecoder,
    pub next_id: i64,
    /// Events that arrived while waiting for a reply.
    pub events: std::collections::VecDeque<Event>,
}

impl Client {
    pub async fn connect(server: &Server) -> Self {
        Client {
            stream: UnixStream::connect(&server.socket).await.unwrap(),
            decoder: FrameDecoder::default(),
            next_id: 1,
            events: Default::default(),
        }
    }

    pub async fn send(&mut self, frame_type: FrameType, channel: u32, payload: &[u8]) {
        self.stream
            .write_all(&encode_frame(frame_type, channel, payload))
            .await
            .unwrap();
    }

    pub async fn send_json(&mut self, frame_type: FrameType, channel: u32, body: &Value) {
        self.send(frame_type, channel, &serde_json::to_vec(body).unwrap())
            .await;
    }

    /// The next frame, or `None` when the server closed the connection.
    pub async fn recv(&mut self) -> Option<(Header, Vec<u8>)> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some(frame) = self.decoder.next_frame(u64::MAX).unwrap() {
                return Some(frame);
            }
            match tokio::time::timeout(Duration::from_secs(5), self.stream.read(&mut buf))
                .await
                .expect("frame within 5 s")
            {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.decoder.push(&buf[..n]),
            }
        }
    }

    pub async fn recv_json(&mut self) -> (Header, Value) {
        let (header, payload) = self.recv().await.expect("a frame");
        (header, serde_json::from_slice(&payload).unwrap())
    }

    /// HELLO naming the server's engine.
    pub async fn hello(&mut self) -> Welcome {
        self.hello_with(Some(ghostty_vt::ENGINE)).await
    }

    pub async fn hello_with(&mut self, engine: Option<&str>) -> Welcome {
        let mut hello =
            json!({"version": 1, "client": {"name": "test", "version": "0"}, "capabilities": []});
        if let Some(engine) = engine {
            hello["engine"] = json!(engine);
        }
        self.send_json(FrameType::Hello, 0, &hello).await;
        let (header, body) = self.recv_json().await;
        assert_eq!(header.frame_type, FrameType::Welcome);
        serde_json::from_value(body).unwrap()
    }

    /// Sends a request and returns its reply or error, queuing events that
    /// arrive first.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, ErrorMessage> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_json(
            FrameType::Request,
            0,
            &json!({"id": id, "method": method, "params": params}),
        )
        .await;
        loop {
            let (header, body) = self.recv_json().await;
            match header.frame_type {
                FrameType::Reply => {
                    assert_eq!(body["id"], id);
                    return Ok(body["result"].clone());
                }
                FrameType::Error => {
                    assert_eq!(body["id"], id, "error for another request: {body}");
                    return Err(serde_json::from_value(body).unwrap());
                }
                FrameType::Event => self.events.push_back(serde_json::from_value(body).unwrap()),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    pub async fn event(&mut self) -> Event {
        if let Some(event) = self.events.pop_front() {
            return event;
        }
        let (header, body) = self.recv_json().await;
        assert_eq!(header.frame_type, FrameType::Event, "{body}");
        serde_json::from_value(body).unwrap()
    }

    pub async fn create(&mut self, params: Value) -> Session {
        serde_json::from_value(self.request("create", params).await.unwrap()).unwrap()
    }

    pub async fn ok(&mut self, method: &str, params: Value) -> Value {
        self.request(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:?}"))
    }

    pub async fn err(&mut self, method: &str, params: Value) -> ErrorCode {
        self.request(method, params).await.expect_err(method).code
    }

    pub async fn wait_exit(&mut self, id: &str) -> Session {
        loop {
            if let Event::SessionExited { session } = self.event().await
                && session.id == id
            {
                return session;
            }
        }
    }
}

pub async fn client(server: &Server) -> Client {
    let mut c = Client::connect(server).await;
    c.hello().await;
    c
}

/// Polls `read` until `needle` appears on the screen.
pub async fn wait_text(c: &mut Client, id: &str, needle: &str) -> Read {
    for _ in 0..200 {
        let read: Read =
            serde_json::from_value(c.ok("read", json!({"session": id})).await).unwrap();
        if read.text.contains(needle) {
            return read;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{needle:?} never appeared");
}
