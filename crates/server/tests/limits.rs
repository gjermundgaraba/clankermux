//! Limits conformance: the rows of PROTOCOL.md's Limits table that no
//! other suite checks (`max_frame` both ways, `scrollback`,
//! `continuation`, `label`, `text`, `markers`, and the default limits
//! against the largest replies).

mod common;

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
    });
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
    });
    let mut c = client(&server).await;
    let session = c
        .create(json!({"rows": 4, "argv": ["/bin/sh", "-c", "seq 1 2000 | sed 's/^/line-/'; echo END; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "END").await;
    let read = c.read(&session.id, 100000).await;
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
    });
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
            json!({"argv": ["/bin/sh", "-c", "a".repeat(30), "a".repeat(30)]})
        )
        .await,
        ErrorCode::LimitExceeded,
        "argv is bounded as a whole: two elements under the limit, over it together"
    );
    // argv is bounded as a whole, so the long title is typed into `cat`,
    // which writes it back to the terminal.
    // The harness's default cwd is a long temporary path.
    let session = c
        .create(json!({"label": "12345678", "argv": ["/bin/cat"], "cwd": "/tmp"}))
        .await;
    let title = "0123456789abcdef".repeat(4);
    let pwd = format!("/{}", "p".repeat(63));
    c.ok(
        "send",
        json!({"session": session.id, "data": b64(format!("\x1b]2;{title}\x07\x1b]7;file://host{pwd}\x07titled\n"))}),
    )
    .await;
    assert_eq!(
        c.err(
            "label",
            json!({"session": session.id, "label": "123456789"})
        )
        .await,
        ErrorCode::LimitExceeded
    );
    // The PTY echoes the typed line (ESC as `^[`) before `cat` writes it
    // back raw, so "titled" appears twice once the title is set.
    let mut echoed = false;
    for _ in 0..1000 {
        if c.read(&session.id, 0).await.text.matches("titled").count() == 2 {
            echoed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(echoed, "cat never wrote the line back");
    let inspected = c.inspect(&session.id).await;
    assert_eq!(inspected.title, title[..48], "title truncated to text");
    assert_eq!(inspected.pwd, pwd[..48], "pwd truncated to text");
}

#[tokio::test(flavor = "multi_thread")]
async fn markers_bound_what_a_resume_can_find() {
    let server = start(Limits {
        markers: 2,
        ..fast_limits()
    });
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
            "resize",
            json!({"session": session.id, "cols": 80, "rows": rows}),
        )
        .await;
        c.expect_resized(1).await;
    }
    c.send(FrameType::Close, 1, &[]).await;
    assert_eq!(c.expect_closed(1).await, ClosedReason::Closed);
    let inspected = c.inspect(&session.id).await;
    assert_eq!(inspected.marker, first + 3);
    // A cursor at `first` has not received `first + 1`, which is gone: the
    // spec requires at least `markers` retained; this server keeps exactly
    // that many, which is what makes the limit observable.
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

/// A record field at the protocol's maxima, with the bytes that JSON
/// escapes to two (OSC 7501 forbids control characters in records).
fn worst(len: usize) -> String {
    "\"".repeat(len)
}

/// A session field at its maximum, with the bytes that JSON escapes to
/// six (`\u0001`); labels, argv and cwd come from clients unfiltered.
fn worst_text(len: usize) -> String {
    "\u{1}".repeat(len)
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
        // Only `done` and `error` carry it; here for the bytes.
        seen: Some(false),
    };
    let attention = Attention {
        state: "blocked".into(),
        kind: Some("permission".into()),
        app: Some(worst(32)),
        msg: Some(worst(2048)),
        record: worst(128),
    };
    // The server cuts the client's name and version to 32 bytes and
    // replaces control characters, so `"` (escaped to two) is the worst.
    let attachment = AttachmentInfo {
        channel: u64::MAX,
        client: Peer {
            name: worst(32),
            version: worst(32),
        },
        read_only: false,
        viewport: Some(Cells {
            cols: u16::MAX,
            rows: u16::MAX,
        }),
        since: "2026-10-18T16:00:00.123456789Z".into(),
        last_input: Some("2026-10-18T16:00:00.123456789Z".into()),
    };
    let session = |records: bool| Session {
        id: "a8098c1a-f86e-11da-bd1a-00112444be1e".into(),
        label: worst_text(limits.label as usize),
        status: Status::Running,
        argv: vec![worst_text(limits.text as usize)],
        cwd: worst_text(limits.text as usize),
        pid: Some(u32::MAX),
        cols: u16::MAX,
        rows: u16::MAX,
        title: worst_text(limits.text as usize),
        pwd: worst_text(limits.text as usize),
        on_exit: OnExit::Remove,
        created_at: "2026-10-18T16:00:00.123456789Z".into(),
        exited_at: Some("2026-10-18T16:00:00.123456789Z".into()),
        exit: Some(Exit::Signal {
            signal: "SIGKILL".into(),
        }),
        offset: u64::MAX,
        marker: u64::MAX,
        attachments: vec![attachment.clone(); limits.attachments as usize],
        command: Some(Command {
            start: u64::MAX,
            output: Some(u64::MAX),
            end: Some(u64::MAX),
            exit: Some(i32::MIN),
        }),
        program_status: records.then(|| vec![record.clone(); limits.records as usize]),
        attention: Some(attention.clone()),
        size: SizeInfo {
            policy: SizePolicy::Smallest,
            owner: Some(u64::MAX),
        },
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
    assert!(
        inspect <= limits.max_frame,
        "inspect reply of {inspect} bytes"
    );
    // A page is cut to what fits (`a_page_is_cut_to_max_frame`), so only
    // one worst session must fit with the page's overhead.
    let one = reply(
        serde_json::to_value(Page {
            sessions: vec![session(false)],
            next: Some("a8098c1a-f86e-11da-bd1a-00112444be1e".into()),
        })
        .unwrap(),
    );
    assert!(one * 2 <= limits.max_frame, "one session of {one} bytes");
}

#[tokio::test(flavor = "multi_thread")]
async fn read_replies_stay_under_max_frame() {
    let server = start(Limits::default());
    let mut c = client(&server).await;
    // 16,000 lines of 79 bytes: over max_frame of text, each newline
    // escaping to two bytes in the reply.
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "awk 'BEGIN { for (i = 0; i < 16000; i++) printf \"%08d %s\\n\", i, \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\" }'; echo END; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "END").await;
    // The harness decodes with the server's max_frame; a larger frame
    // fails the test there.
    let reply = c
        .ok("read", json!({"session": session.id, "scrollback": 100000}))
        .await;
    let read: Read = serde_json::from_value(reply.clone()).unwrap();
    assert!(read.text.ends_with("END\n"), "the newest lines stay");
    assert!(
        read.text.lines().count() > 12000,
        "most of the history stays"
    );
    let encoded = serde_json::to_vec(&json!({"id": 1, "result": reply})).unwrap();
    assert!(encoded.len() as u64 <= Limits::default().max_frame);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_is_cut_to_max_frame() {
    let server = start(Limits {
        page: 100,
        ..Limits::default()
    });
    let mut c = client(&server).await;
    // Sessions whose label and argv escape to six bytes a character: about
    // 14 KiB each, so a page of 100 would be over max_frame.
    let label = "\u{1}".repeat(Limits::default().label as usize);
    let arg = "\u{1}".repeat(2000);
    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..100 {
        let session = c
            .create(json!({"label": label, "argv": ["/bin/sh", "-c", ":", arg]}))
            .await;
        ids.insert(session.id);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let reply = c.ok("list", json!({"after": after})).await;
        let page: Page = serde_json::from_value(reply.clone()).unwrap();
        pages += 1;
        let encoded = serde_json::to_vec(&json!({"id": 1, "result": reply})).unwrap();
        assert!(encoded.len() as u64 <= Limits::default().max_frame);
        assert!(page.sessions.len() < 100, "cut below the page limit");
        for s in &page.sessions {
            assert!(seen.insert(s.id.clone()), "a session listed twice");
        }
        assert_eq!(
            page.next.as_deref(),
            (seen.len() < 100).then(|| page.sessions.last().unwrap().id.as_str())
        );
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, ids);
    assert!(pages >= 2);
    // The first `subscribe` page is cut the same way.
    let reply = c.ok("subscribe", json!({})).await;
    let page: Page = serde_json::from_value(reply.clone()).unwrap();
    let encoded = serde_json::to_vec(&json!({"id": 1, "result": reply})).unwrap();
    assert!(encoded.len() as u64 <= Limits::default().max_frame);
    assert!(page.sessions.len() < 100);
    assert_eq!(
        page.next.as_deref(),
        Some(page.sessions.last().unwrap().id.as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn continuation_bounds_a_snapshot() {
    let server = start(Limits {
        continuation: 4096,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    // An OSC left open with more bytes than the limit tracks.
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c",
            "printf 'up\\033]2;'; head -c 6000 /dev/zero | tr '\\0' a; exec cat"]}))
        .await;
    inspect_until(&mut c, &session.id, |s| s.offset >= 6000).await;
    // No snapshot can be taken for the whole 5 s retry window, so the
    // bootstrap fails and the attachment ends.
    c.open(1, json!({"session": session.id})).await;
    assert_eq!(c.expect_error(1).await, ErrorCode::LimitExceeded);
    c.wait_attachments(&session.id, 0).await;
    // Closing the sequence and writing at ground again makes the session
    // attachable (the engine's continuation stays unavailable until input
    // follows the overflowed sequence).
    c.ok(
        "send",
        json!({"session": session.id, "data": b64("\x07done\n")}),
    )
    .await;
    wait_text(&mut c, &session.id, "done").await;
    c.open(1, json!({"session": session.id})).await;
    c.bootstrap(1).await;
}
