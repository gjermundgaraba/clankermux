//! Conformance tests for the Attach, Size and Queries sections of
//! PROTOCOL.md.

mod common;

use std::time::Duration;

use base64::Engine as _;
use common::*;
use ghostty_vt::{formatter, snapshot};
use proto::*;
use serde_json::{Value, json};

/// The bootstrap of a `snapshot` attachment: ATTACHED, snapshot frames
/// and SNAPSHOT_READY / SNAPSHOT_END markers.
struct Bootstrap {
    attached: Attached,
    /// Snapshot bytes through the READY record.
    ready: Vec<u8>,
    /// The remaining snapshot bytes.
    rest: Vec<u8>,
    /// Stream frames interleaved after SNAPSHOT_READY.
    stream: Vec<(Header, Vec<u8>)>,
}

impl Bootstrap {
    fn terminal(&self) -> ghostty_vt::Terminal {
        let mut bytes = self.ready.clone();
        bytes.extend_from_slice(&self.rest);
        snapshot::decode(&bytes).unwrap()
    }

    fn text(&self) -> String {
        let term = self.terminal();
        let out = formatter::format(
            &term,
            formatter::Options {
                trim: true,
                ..Default::default()
            },
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }
}

trait Attach {
    async fn open(&mut self, channel: u32, body: Value);
    async fn recv_on(&mut self, channel: u32) -> (Header, Vec<u8>);
    async fn attached(&mut self, channel: u32) -> Attached;
    async fn bootstrap(&mut self, channel: u32) -> Bootstrap;
    async fn expect_error(&mut self, channel: u32) -> ErrorCode;
    async fn expect_closed(&mut self, channel: u32) -> ClosedReason;
    async fn output_until(&mut self, channel: u32, needle: &[u8]) -> (u64, Vec<u8>);
}

impl Attach for Client {
    async fn open(&mut self, channel: u32, body: Value) {
        self.send_json(FrameType::Open, channel, &body).await;
    }

    async fn recv_on(&mut self, channel: u32) -> (Header, Vec<u8>) {
        let (header, payload) = self.recv().await.expect("a frame");
        assert_eq!(header.channel, channel, "{header:?}");
        (header, payload)
    }

    async fn attached(&mut self, channel: u32) -> Attached {
        let (header, payload) = self.recv_on(channel).await;
        assert_eq!(
            header.frame_type,
            FrameType::Attached,
            "{}",
            String::from_utf8_lossy(&payload)
        );
        serde_json::from_slice(&payload).unwrap()
    }

    async fn bootstrap(&mut self, channel: u32) -> Bootstrap {
        let attached = self.attached(channel).await;
        assert_eq!(attached.mode, AttachMode::Snapshot);
        let mut ready = Vec::new();
        loop {
            let (header, payload) = self.recv_on(channel).await;
            match header.frame_type {
                FrameType::Snapshot => ready.extend_from_slice(&payload),
                FrameType::SnapshotReady => break,
                other => panic!("{other:?} before SNAPSHOT_READY"),
            }
        }
        assert_eq!(snapshot::ready_end(&ready), Some(ready.len()));
        let mut rest = Vec::new();
        let mut stream = Vec::new();
        loop {
            let (header, payload) = self.recv_on(channel).await;
            match header.frame_type {
                FrameType::Snapshot => rest.extend_from_slice(&payload),
                FrameType::SnapshotEnd => break,
                _ => stream.push((header, payload)),
            }
        }
        Bootstrap {
            attached,
            ready,
            rest,
            stream,
        }
    }

    async fn expect_error(&mut self, channel: u32) -> ErrorCode {
        let (header, payload) = self.recv_on(channel).await;
        assert_eq!(header.frame_type, FrameType::Error);
        let error: ErrorMessage = serde_json::from_slice(&payload).unwrap();
        error.code
    }

    async fn expect_closed(&mut self, channel: u32) -> ClosedReason {
        let (header, payload) = self.recv_on(channel).await;
        assert_eq!(header.frame_type, FrameType::Closed);
        let closed: Closed = serde_json::from_slice(&payload).unwrap();
        closed.reason
    }

    /// Reads OUTPUT frames until `needle` appears; returns the offset of
    /// the first frame and the bytes, asserting contiguous offsets.
    async fn output_until(&mut self, channel: u32, needle: &[u8]) -> (u64, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut first = None;
        loop {
            let (header, payload) = self.recv_on(channel).await;
            assert_eq!(header.frame_type, FrameType::Output);
            let (offset, data) = parse_output(&payload).unwrap();
            let start = *first.get_or_insert(offset);
            assert_eq!(offset, start + bytes.len() as u64, "contiguous offsets");
            bytes.extend_from_slice(data);
            if bytes.windows(needle.len()).any(|w| w == needle) {
                return (start, bytes);
            }
        }
    }
}

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

async fn wait_attachments(c: &mut Client, id: &str, count: u32) {
    for _ in 0..200 {
        let s: Session =
            serde_json::from_value(c.ok("inspect", json!({"session": id})).await).unwrap();
        if s.attachments == count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("attachments never reached {count}");
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_bootstrap_then_stream_then_close() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/cat"], "cols": 20, "rows": 5}))
        .await;
    c.ok(
        "write",
        json!({"session": session.id, "data": b64("one\n")}),
    )
    .await;
    let read = wait_text(&mut c, &session.id, "one\none").await;

    c.open(
        1,
        json!({"session": session.id, "viewport": {"cols": 20, "rows": 5}}),
    )
    .await;
    let boot = c.bootstrap(1).await;
    assert_eq!(boot.attached.offset, read.offset);
    assert_eq!(boot.attached.marker, 0);
    assert_eq!((boot.attached.cols, boot.attached.rows), (20, 5));
    assert_eq!(boot.attached.session.attachments, 1);
    assert!(boot.stream.is_empty(), "no stream frames while idle");
    assert_eq!(boot.text(), "one\none");
    let mut decoder = snapshot::Decoder::new().unwrap();
    decoder.push(&boot.ready);
    assert!(decoder.ready_available());
    assert_eq!(decoder.ready().unwrap().cols(), 20);

    c.send(FrameType::Input, 1, b"two\n").await;
    let (offset, bytes) = c.output_until(1, b"two\r\ntwo\r\n").await;
    assert_eq!(offset, boot.attached.offset);
    assert_eq!(bytes, b"two\r\ntwo\r\n");

    c.send(FrameType::Close, 1, b"").await;
    assert_eq!(c.expect_closed(1).await, ClosedReason::Closed);
    let inspected: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": session.id})).await).unwrap();
    assert_eq!(inspected.attachments, 0);

    // The channel is free again.
    c.open(1, json!({"session": session.id})).await;
    let boot = c.bootstrap(1).await;
    assert_eq!(boot.attached.offset, offset + bytes.len() as u64);
    assert_eq!(boot.text(), "one\none\ntwo\ntwo");
}

#[tokio::test(flavor = "multi_thread")]
async fn open_checks_in_order() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let welcome_incarnation = {
        let mut probe = Client::connect(&server).await;
        probe.hello().await.incarnation
    };
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;

    c.open(2, json!({"session": "nope"})).await;
    assert_eq!(c.expect_error(2).await, ErrorCode::NoSuchSession);

    c.open(3, json!({"session": session.id})).await;
    c.bootstrap(3).await;
    c.open(3, json!({"session": "nope"})).await;
    assert_eq!(
        c.expect_error(3).await,
        ErrorCode::ChannelInUse,
        "checked before the session"
    );
    wait_attachments(&mut c, &session.id, 0).await;

    c.send(FrameType::Open, 4, b"{bad").await;
    assert_eq!(c.expect_error(4).await, ErrorCode::InvalidRequest);

    c.open(
        5,
        json!({"session": session.id, "resume": {"incarnation": welcome_incarnation, "offset": 0, "marker": 99}}),
    )
    .await;
    assert_eq!(c.expect_error(5).await, ErrorCode::InvalidRequest);

    let mut no_engine = Client::connect(&server).await;
    no_engine.hello_with(None).await;
    no_engine.open(1, json!({"session": session.id})).await;
    assert_eq!(no_engine.expect_error(1).await, ErrorCode::EngineMismatch);

    let mut other = Client::connect(&server).await;
    other.hello_with(Some("ghostty-vt@0.0.0")).await;
    other.open(1, json!({"session": session.id})).await;
    assert_eq!(other.expect_error(1).await, ErrorCode::EngineMismatch);
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_when_retained_else_snapshot() {
    let server = start(Limits {
        ring: 64,
        ..fast_limits()
    })
    .await;
    let mut c = client(&server).await;
    let incarnation = {
        let mut probe = Client::connect(&server).await;
        probe.hello().await.incarnation
    };
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    c.open(1, json!({"session": session.id})).await;
    let boot = c.bootstrap(1).await;
    c.send(FrameType::Input, 1, b"a\n").await;
    let (offset, bytes) = c.output_until(1, b"a\r\na\r\n").await;
    assert_eq!(offset, boot.attached.offset);
    let cursor = offset + bytes.len() as u64;
    c.send(FrameType::Close, 1, b"").await;
    c.expect_closed(1).await;

    c.open(
        1,
        json!({"session": session.id, "resume": {"incarnation": incarnation, "offset": cursor, "marker": 0}}),
    )
    .await;
    let attached = c.attached(1).await;
    assert_eq!(attached.mode, AttachMode::Resume);
    assert_eq!((attached.offset, attached.marker), (cursor, 0));
    c.send(FrameType::Input, 1, b"b\n").await;
    let (offset, _) = c.output_until(1, b"b\r\nb\r\n").await;
    assert_eq!(offset, cursor, "the stream continues from the cursor");
    c.send(FrameType::Close, 1, b"").await;
    c.expect_closed(1).await;

    c.open(
        1,
        json!({"session": session.id, "resume": {"incarnation": "other", "offset": cursor, "marker": 0}}),
    )
    .await;
    c.bootstrap(1).await;
    c.send(FrameType::Close, 1, b"").await;
    c.expect_closed(1).await;

    // Push offset 0 out of the 64-byte ring.
    c.ok(
        "write",
        json!({"session": session.id, "data": b64(&"x".repeat(100))}),
    )
    .await;
    wait_text(&mut c, &session.id, &"x".repeat(100)).await;
    c.open(
        1,
        json!({"session": session.id, "resume": {"incarnation": incarnation, "offset": 0, "marker": 0}}),
    )
    .await;
    c.bootstrap(1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn viewport_reports_coalesce_and_mark_the_stream() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/cat"], "cols": 20, "rows": 5}))
        .await;

    c.open(
        1,
        json!({"session": session.id, "viewport": {"cols": 30, "rows": 8}}),
    )
    .await;
    let boot = c.bootstrap(1).await;
    assert_eq!((boot.attached.cols, boot.attached.rows), (30, 8));
    assert_eq!(
        boot.attached.marker, 1,
        "OPEN's viewport applied before the snapshot"
    );
    assert_eq!(boot.terminal().cols(), 30);

    c.send_json(FrameType::Viewport, 1, &json!({"cols": 40, "rows": 10}))
        .await;
    c.send_json(FrameType::Viewport, 1, &json!({"cols": 30, "rows": 8}))
        .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let inspected: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": session.id})).await).unwrap();
    assert_eq!(
        (inspected.cols, inspected.marker),
        (30, 1),
        "latest report wins"
    );

    c.send_json(FrameType::Viewport, 1, &json!({"cols": 50, "rows": 12}))
        .await;
    let (header, payload) = c.recv_on(1).await;
    assert_eq!(header.frame_type, FrameType::Resized);
    let resized: Resized = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        resized,
        Resized {
            seq: 2,
            offset: boot.attached.offset,
            cols: 50,
            rows: 12
        }
    );

    c.ok(
        "set_size",
        json!({"session": session.id, "cols": 60, "rows": 15}),
    )
    .await;
    let (_, payload) = c.recv_on(1).await;
    let resized: Resized = serde_json::from_slice(&payload).unwrap();
    assert_eq!((resized.seq, resized.cols), (3, 60));

    c.open(
        2,
        json!({"session": session.id, "read_only": true, "viewport": {"cols": 1, "rows": 1}}),
    )
    .await;
    let boot = c.bootstrap(2).await;
    assert_eq!(boot.attached.cols, 60, "a read-only viewport is ignored");
    c.send(FrameType::Input, 2, b"x").await;
    assert_eq!(c.expect_error(2).await, ErrorCode::ReadOnly);
    c.send_json(FrameType::Viewport, 2, &json!({"cols": 1, "rows": 1}))
        .await;
    assert_eq!(c.expect_error(2).await, ErrorCode::ReadOnly);
    c.send(FrameType::Close, 2, b"").await;
    assert_eq!(
        c.expect_closed(2).await,
        ClosedReason::Closed,
        "the attachment survived"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_ends_the_stream_and_removal_closes() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let mut events = client(&server).await;
    events.ok("subscribe", json!({})).await;

    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "sleep 0.3; echo bye; exit 3"]}))
        .await;
    c.open(1, json!({"session": session.id})).await;
    let boot = c.bootstrap(1).await;
    let (offset, bytes) = c.output_until(1, b"bye\r\n").await;
    assert_eq!(offset, boot.attached.offset);
    let (header, payload) = c.recv_on(1).await;
    assert_eq!(header.frame_type, FrameType::Exited);
    let exited: Exited = serde_json::from_slice(&payload).unwrap();
    assert_eq!(exited.seq, 1);
    assert_eq!(exited.offset, offset + bytes.len() as u64);
    assert_eq!(exited.exit, Exit::Code { code: 3 });
    c.send(FrameType::Input, 1, b"x").await;
    assert_eq!(c.expect_error(1).await, ErrorCode::NotRunning);
    assert_eq!(
        c.expect_closed(1).await,
        ClosedReason::Removed,
        "on_exit remove after the grace"
    );
    events.wait_exit(&session.id).await;
    assert!(matches!(events.event().await, Event::SessionRemoved { id } if id == session.id));

    let kept = c
        .create(json!({"argv": ["/bin/sh", "-c", "exit 0"], "on_exit": "keep"}))
        .await;
    events.wait_exit(&kept.id).await;
    c.open(2, json!({"session": kept.id})).await;
    let boot = c.bootstrap(2).await;
    assert_eq!(boot.attached.session.status, Status::Exited);
    assert_eq!(
        boot.attached.marker, 1,
        "the snapshot reflects the exit; no EXITED frame"
    );
    assert!(boot.stream.is_empty());
    c.ok("remove", json!({"session": kept.id})).await;
    assert_eq!(
        c.expect_closed(2).await,
        ClosedReason::Removed,
        "finished attachments close at once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gap_when_the_client_falls_behind() {
    let server = start(Limits {
        ring: 16 * 1024,
        ..fast_limits()
    })
    .await;
    let mut c = client(&server).await;
    let mut events = client(&server).await;
    events.ok("subscribe", json!({})).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "sleep 0.3; yes | head -c 1500000"], "on_exit": "keep", "cols": 20, "rows": 5}))
        .await;
    c.open(1, json!({"session": session.id})).await;
    let boot = c.bootstrap(1).await;
    assert!(boot.stream.is_empty());
    // Read nothing until the session has produced everything and exited.
    let exited = events.wait_exit(&session.id).await;
    let mut delivered = 0u64;
    loop {
        let (header, payload) = c.recv_on(1).await;
        match header.frame_type {
            FrameType::Output => {
                let (offset, data) = parse_output(&payload).unwrap();
                assert_eq!(offset, boot.attached.offset + delivered);
                delivered += data.len() as u64;
            }
            FrameType::Gap => {
                let gap: Gap = serde_json::from_slice(&payload).unwrap();
                assert_eq!(gap.from, boot.attached.offset + delivered);
                assert_eq!(gap.to, exited.offset);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(delivered < 1_500_000);
    let again = c.bootstrap(1).await;
    assert_eq!(again.attached.offset, exited.offset);
    assert_eq!(again.attached.session.status, Status::Exited);
    assert!(again.stream.is_empty(), "the snapshot reflects the exit");
}

#[tokio::test(flavor = "multi_thread")]
async fn too_slow_during_a_bootstrap() {
    // Small frames and a history that does not compress, so the snapshot
    // is far larger than the connection's queue allows in flight.
    let server = start(Limits {
        ring: 16 * 1024,
        max_frame: 16 * 1024,
        ..fast_limits()
    })
    .await;
    let mut c = client(&server).await;
    let mut events = client(&server).await;
    events.ok("subscribe", json!({})).await;
    let script =
        "head -c 600000 /dev/urandom | base64; echo FIRST-DONE; sleep 0.5; yes | head -c 100000";
    let session = c
        .create(
            json!({"argv": ["/bin/sh", "-c", script], "on_exit": "keep", "cols": 20, "rows": 5}),
        )
        .await;
    wait_text(&mut c, &session.id, "FIRST-DONE").await;
    c.open(1, json!({"session": session.id})).await;
    let attached = c.attached(1).await;
    assert_eq!(attached.mode, AttachMode::Snapshot);
    loop {
        let (header, _) = c.recv_on(1).await;
        if header.frame_type == FrameType::SnapshotReady {
            break;
        }
    }
    // The history is far larger than the connection's queue; stop reading
    // until the second burst has pushed the cursor out of the ring.
    events.wait_exit(&session.id).await;
    loop {
        let (header, payload) = c.recv_on(1).await;
        match header.frame_type {
            FrameType::Snapshot | FrameType::Output => {}
            FrameType::Error => {
                let error: ErrorMessage = serde_json::from_slice(&payload).unwrap();
                assert_eq!(error.code, ErrorCode::TooSlow);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    wait_attachments(&mut c, &session.id, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn input_overflow_keeps_the_attachment() {
    let server = start(Limits {
        input_queue: 16,
        ..fast_limits()
    })
    .await;
    let mut c = client(&server).await;
    let session = c.create(json!({"argv": ["/bin/sleep", "5"]})).await;
    c.open(1, json!({"session": session.id})).await;
    c.bootstrap(1).await;
    c.send(FrameType::Input, 1, &[b'x'; 32]).await;
    assert_eq!(c.expect_error(1).await, ErrorCode::InputOverflow);
    c.send(FrameType::Close, 1, b"").await;
    assert_eq!(c.expect_closed(1).await, ClosedReason::Closed);
}

#[tokio::test(flavor = "multi_thread")]
async fn queries_are_answered_from_the_authoritative_profile() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "stty raw -echo; exec cat"]}))
        .await;
    // Give stty time to switch the line discipline.
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.open(
        1,
        json!({"session": session.id, "profile": {"scheme": "dark", "bg": "#102030"}}),
    )
    .await;
    c.bootstrap(1).await;

    c.send(FrameType::Input, 1, b"\x1b[>0q").await;
    c.output_until(1, b"\x1bP>|clankermux test\x1b\\").await;
    c.send(FrameType::Input, 1, b"\x1b[?996n").await;
    c.output_until(1, b"\x1b[?997;1n").await;
    c.send(FrameType::Input, 1, b"\x1b]11;?\x1b\\").await;
    c.output_until(1, b"\x1b]11;rgb:1010/2020/3030\x1b\\").await;
    c.send(FrameType::Input, 1, b"\x1b[?2031h").await;

    // A second writable attachment's profile becomes authoritative; with
    // mode 2031 set the changed answer is reported unasked.
    let mut other = client(&server).await;
    other
        .open(
            7,
            json!({"session": session.id, "profile": {"scheme": "light"}}),
        )
        .await;
    other.bootstrap(7).await;
    c.output_until(1, b"\x1b[?997;2n").await;
    c.send(FrameType::Input, 1, b"\x1b[?996n").await;
    c.output_until(1, b"\x1b[?997;2n").await;

    // Without a scheme the background decides.
    other
        .send_json(
            FrameType::Viewport,
            7,
            &json!({"cols": 80, "rows": 24, "profile": {"bg": "#000000"}}),
        )
        .await;
    c.output_until(1, b"\x1b[?997;1n").await;
}
