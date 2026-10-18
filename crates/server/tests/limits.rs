//! Limits conformance: one check per row of PROTOCOL.md's Limits table
//! that earlier suites do not already cover. `ring` and `input_queue`
//! are in attach.rs, `page`, `hello_timeout`, `remove_grace` and
//! `control_body` in control.rs, `records` in park.rs.

mod common;

use base64::Engine as _;
use common::*;
use proto::*;
use serde_json::json;
use tokio::io::AsyncWriteExt;

#[tokio::test(flavor = "multi_thread")]
async fn max_frame_bounds_every_frame() {
    // Above the 4 KiB of JSON a `read` reply keeps around its text.
    const MAX: usize = 16 * 1024;
    let server = start(Limits {
        max_frame: MAX as u64,
        ..fast_limits()
    })
    .await;
    let mut c = client(&server).await;
    // A payload of max_frame bytes passes, one more closes the connection.
    let mut padded = br#"{"id": 1, "method": "list", "params": {"after": ""#.to_vec();
    padded.resize(MAX - 3, b'x');
    padded.extend(br#""}}"#);
    c.stream
        .write_all(&encode_frame(FrameType::Request, 0, &padded))
        .await
        .unwrap();
    let (_, body) = c.recv_json().await;
    assert_eq!(body["id"], 1);
    // The server may close on the header before the payload is written.
    let _ = c
        .stream
        .write_all(&encode_frame(FrameType::Request, 0, &[b' '; MAX + 1]))
        .await;
    let (header, body) = c.recv_json().await;
    assert_eq!(header.channel, 0);
    assert_eq!(body["code"], "protocol_error");
    assert!(body.get("id").is_none(), "a connection error has no id");
    assert!(c.recv().await.is_none());

    // Server frames are bounded too: a snapshot arrives in max_frame pieces.
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "seq 1 3000; echo END; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "END").await;
    c.open(1, json!({"session": session.id})).await;
    let mut snapshot = 0;
    loop {
        let (header, _) = c.recv_on(1).await;
        assert!(header.length as usize <= MAX, "{header:?}");
        if header.frame_type == FrameType::Snapshot {
            snapshot += header.length as usize;
        }
        if header.frame_type == FrameType::SnapshotEnd {
            break;
        }
    }
    assert!(snapshot > MAX, "a real snapshot spans frames");
}

#[tokio::test(flavor = "multi_thread")]
async fn scrollback_bounds_the_history() {
    let server = start(Limits {
        scrollback: 4096,
        ..fast_limits()
    })
    .await;
    let mut c = client(&server).await;
    let session = c
        .create(json!({"rows": 4, "argv": ["/bin/sh", "-c", "seq 1 2000 | sed 's/^/line-/'; echo END; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "END").await;
    let read: Read = serde_json::from_value(
        c.ok("read", json!({"session": session.id, "scrollback": 100000}))
            .await,
    )
    .unwrap();
    let lines = read.text.lines().count();
    assert!(lines < 2000, "{lines} lines retained");
    assert!(
        read.text.contains("line-2000\nEND"),
        "the newest history stays"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn label_and_text_are_bounded_everywhere() {
    let server = start(Limits {
        label: 8,
        text: 48,
        ..fast_limits()
    })
    .await;
    let mut c = client(&server).await;
    let long = "a".repeat(49);
    assert_eq!(
        c.err("create", json!({"label": "123456789"})).await,
        ErrorCode::LimitExceeded
    );
    assert_eq!(
        c.err("create", json!({"cwd": format!("/tmp/{long}")}))
            .await,
        ErrorCode::LimitExceeded
    );
    assert_eq!(
        c.err(
            "create",
            json!({"argv": ["/bin/sh", "-c", format!("echo {long}")]})
        )
        .await,
        ErrorCode::LimitExceeded,
        "argv is bounded as a whole"
    );
    // argv is bounded as a whole, so the long title is typed into `cat`,
    // which writes it back to the terminal.
    // The harness's default cwd is a long temporary path.
    let session = c
        .create(json!({"label": "12345678", "argv": ["/bin/cat"], "cwd": "/tmp"}))
        .await;
    let title = "0123456789abcdef".repeat(4);
    c.ok(
        "write",
        json!({"session": session.id, "data": base64::engine::general_purpose::STANDARD
            .encode(format!("\x1b]2;{title}\x07titled\n"))}),
    )
    .await;
    assert_eq!(
        c.err(
            "set_label",
            json!({"session": session.id, "label": "123456789"})
        )
        .await,
        ErrorCode::LimitExceeded
    );
    // The PTY echoes the typed line (ESC as `^[`) before `cat` writes it
    // back raw, so "titled" appears twice once the title is set.
    for _ in 0..200 {
        let read: Read =
            serde_json::from_value(c.ok("read", json!({"session": session.id})).await).unwrap();
        if read.text.matches("titled").count() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let inspected: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": session.id})).await).unwrap();
    assert_eq!(inspected.title, title[..48], "title truncated to text");
    assert!(inspected.pwd.len() <= 48);
}

#[tokio::test(flavor = "multi_thread")]
async fn markers_bound_what_a_resume_can_find() {
    let server = start(Limits {
        markers: 2,
        ..fast_limits()
    })
    .await;
    let mut c = client(&server).await;
    let incarnation = server.registry.incarnation.clone();
    let session = c.create(json!({"argv": ["/bin/cat"]})).await;
    c.open(
        1,
        json!({"session": session.id, "viewport": {"cols": 80, "rows": 24}}),
    )
    .await;
    let boot = c.bootstrap(1).await;
    let first = boot.attached.marker;
    // Three resizes add three markers; only the last two are retained.
    for rows in [10, 11, 12] {
        c.ok(
            "set_size",
            json!({"session": session.id, "cols": 80, "rows": rows}),
        )
        .await;
        c.expect_resized(1).await;
    }
    c.send(FrameType::Close, 1, &[]).await;
    assert_eq!(c.expect_closed(1).await, ClosedReason::Closed);
    let inspected: Session =
        serde_json::from_value(c.ok("inspect", json!({"session": session.id})).await).unwrap();
    assert_eq!(inspected.marker, first + 3);
    // A cursor at `first` has not received `first + 1`, which is gone.
    c.open(
        2,
        json!({"session": session.id, "viewport": {"cols": 80, "rows": 12},
        "resume": {"incarnation": incarnation, "offset": inspected.offset, "marker": first}}),
    )
    .await;
    assert_eq!(
        c.bootstrap(2).await.attached.mode,
        AttachMode::Snapshot,
        "marker no longer retained"
    );
    c.send(FrameType::Close, 2, &[]).await;
    assert_eq!(c.expect_closed(2).await, ClosedReason::Closed);
    // One past that has received `first + 1`; the two after it remain.
    c.open(
        3,
        json!({"session": session.id, "viewport": {"cols": 80, "rows": 12},
        "resume": {"incarnation": incarnation, "offset": inspected.offset, "marker": first + 1}}),
    )
    .await;
    assert_eq!(
        c.attached(3).await.mode,
        AttachMode::Resume,
        "within the retained markers"
    );
}

/// A record or session at the protocol's maxima, with the bytes that
/// JSON escapes to two.
fn worst(len: usize) -> String {
    "\"".repeat(len)
}

#[test]
fn default_limits_keep_the_largest_replies_under_max_frame() {
    let limits = Limits::default();
    let record = StatusRecord {
        id: worst(128),
        state: "blocked".into(),
        kind: Some("permission".into()),
        app: Some(worst(32)),
        title: Some(worst(192)),
        msg: Some(worst(2048)),
        progress: Some(100),
        updated_at: "2026-10-18T16:00:00.123456789Z".into(),
    };
    let session = |records: bool| Session {
        id: "a8098c1a-f86e-11da-bd1a-00112444be1e".into(),
        label: worst(limits.label as usize),
        status: Status::Running,
        argv: vec![worst(limits.text as usize)],
        cwd: worst(limits.text as usize),
        pid: Some(u32::MAX),
        cols: u16::MAX,
        rows: u16::MAX,
        title: worst(limits.text as usize),
        pwd: worst(limits.text as usize),
        on_exit: OnExit::Remove,
        created_at: "2026-10-18T16:00:00.123456789Z".into(),
        exited_at: Some("2026-10-18T16:00:00.123456789Z".into()),
        exit: Some(Exit::Signal {
            signal: "SIGKILL".into(),
        }),
        offset: u64::MAX,
        marker: u64::MAX,
        attachments: u32::MAX,
        program_status: records.then(|| vec![record.clone(); limits.records as usize]),
    };
    let reply = |result: serde_json::Value| {
        serde_json::to_vec(&Reply {
            id: i64::MAX,
            result,
        })
        .unwrap()
        .len() as u64
    };
    let inspect = reply(serde_json::to_value(session(true)).unwrap());
    let page = reply(
        serde_json::to_value(Page {
            sessions: vec![session(false); limits.page as usize],
            next: Some("a8098c1a-f86e-11da-bd1a-00112444be1e".into()),
        })
        .unwrap(),
    );
    assert!(
        inspect <= limits.max_frame,
        "inspect reply of {inspect} bytes"
    );
    assert!(
        page <= limits.max_frame,
        "list or subscribe reply of {page} bytes"
    );
}
