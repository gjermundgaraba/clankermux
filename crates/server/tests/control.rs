//! Conformance tests for the Control section of PROTOCOL.md, driven over
//! a Unix socket against an in-process server.

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use proto::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

struct Server {
    socket: PathBuf,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start(limits: Limits) -> Server {
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

fn fast_limits() -> Limits {
    Limits {
        hello_timeout: 300,
        remove_grace: 200,
        page: 2,
        ..Limits::default()
    }
}

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    next_id: u64,
    /// Events that arrived while waiting for a reply.
    events: std::collections::VecDeque<Event>,
}

impl Client {
    async fn connect(server: &Server) -> Self {
        Client {
            stream: UnixStream::connect(&server.socket).await.unwrap(),
            decoder: FrameDecoder::default(),
            next_id: 1,
            events: Default::default(),
        }
    }

    async fn send(&mut self, frame_type: FrameType, channel: u32, payload: &[u8]) {
        self.stream
            .write_all(&encode_frame(frame_type, channel, payload))
            .await
            .unwrap();
    }

    async fn send_json(&mut self, frame_type: FrameType, channel: u32, body: &Value) {
        self.send(frame_type, channel, &serde_json::to_vec(body).unwrap())
            .await;
    }

    /// The next frame, or `None` when the server closed the connection.
    async fn recv(&mut self) -> Option<(Header, Vec<u8>)> {
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

    async fn recv_json(&mut self) -> (Header, Value) {
        let (header, payload) = self.recv().await.expect("a frame");
        (header, serde_json::from_slice(&payload).unwrap())
    }

    async fn hello(&mut self) -> Welcome {
        self.send_json(
            FrameType::Hello,
            0,
            &json!({"version": 1, "client": {"name": "test", "version": "0"}, "capabilities": []}),
        )
        .await;
        let (header, body) = self.recv_json().await;
        assert_eq!(header.frame_type, FrameType::Welcome);
        serde_json::from_value(body).unwrap()
    }

    /// Sends a request and returns its reply or error, queuing events that
    /// arrive first.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ErrorMessage> {
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

    async fn event(&mut self) -> Event {
        if let Some(event) = self.events.pop_front() {
            return event;
        }
        let (header, body) = self.recv_json().await;
        assert_eq!(header.frame_type, FrameType::Event, "{body}");
        serde_json::from_value(body).unwrap()
    }

    async fn create(&mut self, params: Value) -> Session {
        serde_json::from_value(self.request("create", params).await.unwrap()).unwrap()
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        self.request(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:?}"))
    }

    async fn err(&mut self, method: &str, params: Value) -> ErrorCode {
        self.request(method, params).await.expect_err(method).code
    }

    async fn wait_exit(&mut self, id: &str) -> Session {
        loop {
            if let Event::SessionExited { session } = self.event().await
                && session.id == id
            {
                return session;
            }
        }
    }
}

async fn client(server: &Server) -> Client {
    let mut c = Client::connect(server).await;
    c.hello().await;
    c
}

#[tokio::test]
async fn handshake_and_protocol_errors() {
    let server = start(fast_limits()).await;

    let mut c = Client::connect(&server).await;
    let welcome = c.hello().await;
    assert_eq!(welcome.version, 1);
    assert_eq!(welcome.engine, ghostty_vt::ENGINE);
    assert_eq!(welcome.limits.page, 2);
    c.send_json(
        FrameType::Hello,
        0,
        &json!({"version": 1, "client": {"name": "t", "version": "0"}}),
    )
    .await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "protocol_error");
    assert!(
        c.recv().await.is_none(),
        "connection closes after a second HELLO"
    );

    let mut c = Client::connect(&server).await;
    c.send_json(
        FrameType::Hello,
        0,
        &json!({"version": 2, "client": {"name": "t", "version": "0"}}),
    )
    .await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "version_mismatch");
    assert!(c.recv().await.is_none());

    let mut c = Client::connect(&server).await;
    c.send_json(
        FrameType::Request,
        0,
        &json!({"id": 1, "method": "list", "params": {}}),
    )
    .await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "protocol_error", "request before HELLO");
    assert!(c.recv().await.is_none());

    let mut c = client(&server).await;
    c.send(FrameType::Request, 3, b"{}").await;
    let (header, body) = c.recv_json().await;
    assert_eq!(
        (header.channel, body["code"].as_str()),
        (0, Some("protocol_error")),
        "REQUEST on channel 3"
    );
    assert!(c.recv().await.is_none());

    let mut c = client(&server).await;
    c.stream
        .write_all(&[0x98, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        .await
        .unwrap();
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "protocol_error", "unknown type");
    assert!(c.recv().await.is_none());

    let mut c = client(&server).await;
    let big = vec![b' '; Limits::default().control_body as usize + 1];
    c.send(FrameType::Request, 0, &big).await;
    let (_, body) = c.recv_json().await;
    assert_eq!(body["code"], "protocol_error", "JSON above control_body");

    let mut c = client(&server).await;
    c.send(FrameType::Input, 9, b"dropped").await;
    assert_eq!(
        c.err("nope", json!({})).await,
        ErrorCode::InvalidRequest,
        "unknown method after a dropped frame"
    );

    let mut c = Client::connect(&server).await;
    assert!(
        c.recv().await.is_none(),
        "closed without HELLO within hello_timeout"
    );
}

#[tokio::test]
async fn session_lifecycle() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let mut events = client(&server).await;
    let first: Page = serde_json::from_value(events.ok("subscribe", json!({})).await).unwrap();
    assert!(first.sessions.is_empty());

    let session = c.create(json!({"label": "cat", "argv": ["/bin/cat"], "cols": 30, "rows": 10, "on_exit": "keep"})).await;
    assert_eq!(session.status, Status::Running);
    assert_eq!((session.cols, session.rows), (30, 10));
    assert_eq!(session.argv, ["/bin/cat"]);
    assert!(session.pid.is_some());
    assert_eq!(session.on_exit, OnExit::Keep);
    assert_eq!(session.marker, 0);
    assert!(
        matches!(events.event().await, Event::SessionCreated { session: s } if s.id == session.id)
    );

    let data = base64::engine::general_purpose::STANDARD.encode("hello\n");
    c.ok("write", json!({"session": session.id, "data": data}))
        .await;
    let mut read = Read::default_read();
    for _ in 0..50 {
        read = serde_json::from_value(c.ok("read", json!({"session": session.id})).await).unwrap();
        if read.text.contains("hello\nhello") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(read.text, "hello\nhello\n", "echo and cat's copy");
    assert_eq!(read.cursor, Some(Cursor { x: 0, y: 2 }));
    assert!(read.offset > 0);

    let listed: Page = serde_json::from_value(c.ok("list", json!({})).await).unwrap();
    assert_eq!(listed.sessions[0].offset, read.offset);
    assert!(listed.sessions[0].program_status.is_none());
    let inspected: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": session.id})).await).unwrap();
    assert_eq!(inspected.program_status, Some(vec![]));

    c.ok(
        "set_size",
        json!({"session": session.id, "cols": 40, "rows": 12}),
    )
    .await;
    assert!(
        matches!(events.event().await, Event::SessionChanged { session: s } if (s.cols, s.rows, s.marker) == (40, 12, 1))
    );
    c.ok(
        "set_size",
        json!({"session": session.id, "cols": 40, "rows": 12}),
    )
    .await;

    let relabelled: Session = serde_json::from_value(
        c.ok("set_label", json!({"session": session.id, "label": "new"}))
            .await,
    )
    .unwrap();
    assert_eq!(relabelled.label, "new");
    assert!(
        matches!(events.event().await, Event::SessionChanged { session: s } if s.label == "new")
    );
    assert_eq!(
        c.err(
            "set_label",
            json!({"session": session.id, "label": "x".repeat(257)})
        )
        .await,
        ErrorCode::LimitExceeded
    );

    assert_eq!(
        c.err("remove", json!({"session": session.id})).await,
        ErrorCode::Running
    );
    assert_eq!(
        c.err("signal", json!({"session": session.id, "signal": "BOGUS"}))
            .await,
        ErrorCode::InvalidRequest
    );
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
    let exited = events.wait_exit(&session.id).await;
    assert_eq!(exited.status, Status::Exited);
    assert_eq!(
        exited.exit,
        Some(Exit::Signal {
            signal: "KILL".into()
        })
    );
    assert_eq!(exited.marker, 2);
    assert!(exited.pid.is_none());

    assert_eq!(
        c.err("write", json!({"session": session.id, "data": ""}))
            .await,
        ErrorCode::NotRunning
    );
    assert_eq!(
        c.err("end", json!({"session": session.id})).await,
        ErrorCode::NotRunning
    );
    assert_eq!(
        c.err(
            "set_size",
            json!({"session": session.id, "cols": 1, "rows": 1})
        )
        .await,
        ErrorCode::NotRunning
    );
    let read: Read = serde_json::from_value(
        c.ok("read", json!({"session": session.id, "scrollback": 5}))
            .await,
    )
    .unwrap();
    assert_eq!(read.status, Status::Exited);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        c.ok("inspect", json!({"session": session.id}))
            .await
            .is_object(),
        "on_exit keep leaves it listed"
    );

    c.ok("remove", json!({"session": session.id})).await;
    assert!(matches!(events.event().await, Event::SessionRemoved { id } if id == session.id));
    assert_eq!(
        c.err("inspect", json!({"session": session.id})).await,
        ErrorCode::NoSuchSession
    );
}

trait DefaultRead {
    fn default_read() -> Read;
}
impl DefaultRead for Read {
    fn default_read() -> Read {
        Read {
            text: String::new(),
            cols: 0,
            rows: 0,
            cursor: None,
            offset: 0,
            status: Status::Running,
        }
    }
}

#[tokio::test]
async fn exit_code_and_removal_on_exit() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c.create(json!({"argv": ["/bin/sh", "-c", "exit 3"]})).await;
    assert!(matches!(c.event().await, Event::SessionCreated { .. }));
    let exited = c.wait_exit(&session.id).await;
    assert_eq!(exited.exit, Some(Exit::Code { code: 3 }));
    assert!(exited.exited_at.is_some());
    let read: Read =
        serde_json::from_value(c.ok("read", json!({"session": session.id})).await).unwrap();
    assert_eq!(read.status, Status::Exited);
    assert!(
        matches!(c.event().await, Event::SessionRemoved { id } if id == session.id),
        "removed after remove_grace"
    );
    assert_eq!(
        c.err("read", json!({"session": session.id})).await,
        ErrorCode::NoSuchSession
    );
}

#[tokio::test]
async fn end_sends_hangup() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c
        .create(
            json!({"argv": ["/bin/sh", "-c", "trap 'exit 7' HUP; while :; do sleep 0.05; done"]}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    c.ok("end", json!({"session": session.id})).await;
    let exited = c.wait_exit(&session.id).await;
    assert_eq!(exited.exit, Some(Exit::Code { code: 7 }));
}

#[tokio::test]
async fn create_defaults_and_limits() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let session = c.create(json!({})).await;
    assert_eq!(session.argv, ["/bin/sh"]);
    assert_eq!((session.cols, session.rows), (80, 24));
    assert_eq!(session.on_exit, OnExit::Remove);
    assert_eq!(session.label, "");
    assert_eq!(
        c.err("create", json!({"label": "x".repeat(300)})).await,
        ErrorCode::LimitExceeded
    );
    assert_eq!(
        c.err("create", json!({"cwd": "x".repeat(3000)})).await,
        ErrorCode::LimitExceeded
    );
    assert_eq!(
        c.err("create", json!({"argv": ["/nonexistent/program"]}))
            .await,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        c.err("create", json!({"cols": "wide"})).await,
        ErrorCode::InvalidRequest
    );

    let env = c.create(json!({"argv": ["/bin/sh", "-c", "printf '%s|%s' \"$TERM\" \"$FOO\"; sleep 1"], "env": {"FOO": "bar"}})).await;
    let mut text = String::new();
    for _ in 0..50 {
        let read: Read =
            serde_json::from_value(c.ok("read", json!({"session": env.id})).await).unwrap();
        text = read.text;
        if text.contains('|') {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(text.trim_end(), "xterm-ghostty|bar");
    c.ok("signal", json!({"session": env.id, "signal": "KILL"}))
        .await;
}

#[tokio::test]
async fn list_pages_by_id() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..5 {
        ids.push(c.create(json!({"argv": ["/bin/sleep", "5"]})).await.id);
    }
    ids.sort();
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page: Page =
            serde_json::from_value(c.ok("list", json!({"after": after})).await).unwrap();
        assert!(page.sessions.len() <= 2);
        seen.extend(page.sessions.iter().map(|s| s.id.clone()));
        match page.next {
            Some(next) => {
                assert_eq!(&next, seen.last().unwrap());
                after = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(seen, ids);
    for id in ids {
        c.ok("signal", json!({"session": id, "signal": "KILL"}))
            .await;
    }
}

#[tokio::test]
async fn events_are_ordered_with_replies() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    c.ok("subscribe", json!({})).await;
    let session = c.create(json!({"argv": ["/bin/sleep", "5"]})).await;
    // The server emits the event before it sends the reply, and this
    // connection receives both in that order.
    assert!(matches!(c.event().await, Event::SessionCreated { session: s } if s.id == session.id));
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
    c.wait_exit(&session.id).await;
}

#[tokio::test]
async fn read_returns_scrollback_then_screen() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "i=1; while [ $i -le 50 ]; do echo line$i; i=$((i+1)); done; sleep 5"], "cols": 20, "rows": 5}))
        .await;
    let mut lines = Vec::new();
    for _ in 0..100 {
        let read: Read = serde_json::from_value(
            c.ok("read", json!({"session": session.id, "scrollback": 3}))
                .await,
        )
        .unwrap();
        lines = read.text.lines().map(str::to_string).collect();
        if lines.last().is_some_and(|l| l == "line50") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(lines.len(), 8, "{lines:?}");
    assert_eq!(lines[0], "line43");
    let read: Read =
        serde_json::from_value(c.ok("read", json!({"session": session.id})).await).unwrap();
    assert_eq!(read.text.lines().count(), 5);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
}
