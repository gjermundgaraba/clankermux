//! Conformance tests for the Attach, Size and Queries sections of
//! PROTOCOL.md.

mod common;

use std::time::Duration;

use common::*;
use ghostty_vt::snapshot;
use proto::*;
use serde_json::json;
use tokio::io::AsyncWriteExt;

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_bootstrap_then_stream_then_close() {
    let server = start(fast_limits());
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
    // The attachment is gone and the channel free once CLOSED is read.
    let inspected = c.inspect(&session.id).await;
    assert_eq!(inspected.attachments, 0);
    c.open(1, json!({"session": session.id})).await;
    let boot = c.bootstrap(1).await;
    assert_eq!(boot.attached.offset, offset + bytes.len() as u64);
    assert_eq!(boot.text(), "one\none\ntwo\ntwo");
}

#[tokio::test(flavor = "multi_thread")]
async fn open_checks_in_order() {
    let server = start(fast_limits());
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
    c.wait_attachments(&session.id, 0).await;

    // A channel is in use until CLOSED. A pipelined CLOSE + OPEN gets
    // CLOSED first, then either `channel_in_use`, when the OPEN was read
    // before the attachment had ended, or the new attachment; nothing of
    // the new attachment precedes the old one's CLOSED.
    c.open(3, json!({"session": session.id})).await;
    c.bootstrap(3).await;
    let mut both = encode_frame(FrameType::Close, 3, b"");
    both.extend(encode_frame(
        FrameType::Open,
        3,
        &serde_json::to_vec(&json!({"session": session.id})).unwrap(),
    ));
    c.stream.write_all(&both).await.unwrap();
    assert_eq!(c.expect_closed(3).await, ClosedReason::Closed);
    let (header, payload) = c.recv_on(3).await;
    match header.frame_type {
        FrameType::Error => {
            let error: ErrorMessage = serde_json::from_slice(&payload).unwrap();
            assert_eq!(error.code, ErrorCode::ChannelInUse);
        }
        FrameType::Attached => {
            let attached = serde_json::from_slice(&payload).unwrap();
            let boot = c.ready_after(3, attached).await;
            c.bootstrap_rest(3, boot).await;
            c.send(FrameType::Close, 3, b"").await;
            assert_eq!(c.expect_closed(3).await, ClosedReason::Closed);
        }
        other => panic!("{other:?} after CLOSED"),
    }
    c.wait_attachments(&session.id, 0).await;

    c.send(FrameType::Open, 4, b"{bad").await;
    assert_eq!(c.expect_error(4).await, ErrorCode::InvalidRequest);
    c.open(
        4,
        json!({"session": session.id, "viewport": {"cols": 0, "rows": 5}}),
    )
    .await;
    assert_eq!(c.expect_error(4).await, ErrorCode::InvalidRequest);

    c.open(
        5,
        json!({"session": session.id, "resume": {"incarnation": welcome_incarnation, "offset": 0, "marker": 99}}),
    )
    .await;
    assert_eq!(c.expect_error(5).await, ErrorCode::InvalidRequest);

    let mut no_engine = Client::connect(&server).await;
    no_engine.hello_with(None).await;
    no_engine.open(1, json!({"session": "nope"})).await;
    assert_eq!(
        no_engine.expect_error(1).await,
        ErrorCode::NoSuchSession,
        "checked before the engine"
    );
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
    });
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

    // A marker added while detached is delivered on resume.
    c.ok(
        "set_size",
        json!({"session": session.id, "cols": 30, "rows": 8}),
    )
    .await;
    c.open(
        1,
        json!({"session": session.id, "resume": {"incarnation": incarnation, "offset": cursor, "marker": 0}}),
    )
    .await;
    let attached = c.attached(1).await;
    assert_eq!(attached.mode, AttachMode::Resume);
    assert_eq!((attached.offset, attached.marker), (cursor, 0));
    assert_eq!((attached.cols, attached.rows), (30, 8));
    assert_eq!(
        c.expect_resized(1).await,
        Resized {
            seq: 1,
            offset: cursor,
            cols: 30,
            rows: 8
        }
    );
    c.send(FrameType::Input, 1, b"b\n").await;
    let (offset, _) = c.output_until(1, b"b\r\nb\r\n").await;
    assert_eq!(offset, cursor, "the stream continues from the cursor");
    c.send(FrameType::Close, 1, b"").await;
    c.expect_closed(1).await;

    c.open(
        1,
        json!({"session": session.id, "resume": {"incarnation": "other", "offset": cursor, "marker": 1}}),
    )
    .await;
    c.bootstrap(1).await;
    c.send(FrameType::Close, 1, b"").await;
    c.expect_closed(1).await;

    // Push offset 0 out of the 64-byte ring.
    c.ok(
        "write",
        json!({"session": session.id, "data": b64("x".repeat(100))}),
    )
    .await;
    wait_text(&mut c, &session.id, &"x".repeat(100)).await;
    c.open(
        1,
        json!({"session": session.id, "resume": {"incarnation": incarnation, "offset": 0, "marker": 1}}),
    )
    .await;
    c.bootstrap(1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn viewport_reports_coalesce_and_mark_the_stream() {
    let server = start(fast_limits());
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

    c.send_json(
        FrameType::Viewport,
        1,
        &json!({"cols": 40, "rows": 10, "cell_width": 9, "cell_height": 18}),
    )
    .await;
    c.send_json(FrameType::Viewport, 1, &json!({"cols": 30, "rows": 8}))
        .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let inspected = c.inspect(&session.id).await;
    assert_eq!(
        (inspected.cols, inspected.marker),
        (30, 1),
        "latest report wins"
    );

    c.send_json(FrameType::Viewport, 1, &json!({"cols": 50, "rows": 12}))
        .await;
    assert_eq!(
        c.expect_resized(1).await,
        Resized {
            seq: 2,
            offset: boot.attached.offset,
            cols: 50,
            rows: 12
        }
    );

    // A report applied at once discards the pending coalesced one.
    c.send_json(FrameType::Viewport, 1, &json!({"cols": 45, "rows": 11}))
        .await;
    c.ok(
        "set_size",
        json!({"session": session.id, "cols": 60, "rows": 15}),
    )
    .await;
    let resized = c.expect_resized(1).await;
    assert_eq!((resized.seq, resized.cols), (3, 60));
    tokio::time::sleep(Duration::from_millis(80)).await;
    let inspected = c.inspect(&session.id).await;
    assert_eq!((inspected.cols, inspected.marker), (60, 3));

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
    c.send(FrameType::Viewport, 2, b"{bad").await;
    assert_eq!(
        c.expect_error(2).await,
        ErrorCode::InvalidRequest,
        "a malformed VIEWPORT ends the attachment"
    );
    c.wait_attachments(&session.id, 1).await;
    c.send(FrameType::Close, 2, b"").await;

    c.send_json(FrameType::Viewport, 1, &json!({"cols": 0, "rows": 1}))
        .await;
    assert_eq!(c.expect_error(1).await, ErrorCode::InvalidRequest);
    c.wait_attachments(&session.id, 0).await;
    let inspected = c.inspect(&session.id).await;
    assert_eq!(inspected.cols, 60);
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_ends_the_stream_and_removal_closes() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let mut events = client(&server).await;
    events.ok("subscribe", json!({})).await;

    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "read go; echo bye; exit 3"]}))
        .await;
    c.open(1, json!({"session": session.id})).await;
    let boot = c.bootstrap(1).await;
    c.send(FrameType::Input, 1, b"go\n").await;
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

    // With a grace far beyond the receive timeout, only an immediate
    // CLOSED can arrive.
    let server = start(Limits {
        remove_grace: 60_000,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    let mut events = client(&server).await;
    events.ok("subscribe", json!({})).await;
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
    // From another connection: the reply and the CLOSED are unordered.
    events.ok("remove", json!({"session": kept.id})).await;
    assert_eq!(
        c.expect_closed(2).await,
        ClosedReason::Removed,
        "finished attachments close at once"
    );
    loop {
        if let Event::SessionRemoved { id } = events.event().await {
            assert_eq!(id, kept.id);
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gap_when_the_client_falls_behind() {
    // The first burst (1.05 MB with CRLFs) fits the ring but not what the
    // connection holds in flight (its queue and the socket buffer, which
    // is larger on Linux), so the cursor stops in it; the second, larger
    // than the ring, evicts the stopped cursor, not one the server is
    // still forwarding.
    let server = start(Limits {
        ring: 1536 * 1024,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    let mut events = client(&server).await;
    events.ok("subscribe", json!({})).await;
    let script = "read go; yes | head -c 700000; echo FIRST-DONE; \
                  read go; yes | head -c 1500000";
    let session = c
        .create(
            json!({"argv": ["/bin/sh", "-c", script], "on_exit": "keep", "cols": 20, "rows": 5}),
        )
        .await;
    let go = json!({"session": session.id, "data": b64("go\n")});
    c.open(1, json!({"session": session.id})).await;
    let boot = c.bootstrap(1).await;
    assert!(boot.stream.is_empty());
    // Read nothing until the session has produced everything and exited.
    events.ok("write", go.clone()).await;
    wait_text(&mut events, &session.id, "FIRST-DONE").await;
    events.ok("write", go).await;
    let exited = events.wait_exit(&session.id).await;
    let mut delivered = 0u64;
    let gap = loop {
        let (header, payload) = c.recv_on(1).await;
        match header.frame_type {
            FrameType::Output => {
                let (offset, data) = parse_output(&payload).unwrap();
                assert_eq!(offset, boot.attached.offset + delivered);
                delivered += data.len() as u64;
            }
            FrameType::Gap => break serde_json::from_slice::<Gap>(&payload).unwrap(),
            other => panic!("{other:?}"),
        }
    };
    assert!(delivered < 1_050_000);
    assert_eq!(gap.from, boot.attached.offset + delivered);
    let again = c.bootstrap(1).await;
    assert_eq!(gap.to, again.attached.offset);
    assert_eq!(again.attached.offset, exited.offset);
    assert_eq!(again.attached.session.status, Status::Exited);
    assert!(again.stream.is_empty(), "the snapshot reflects the exit");
}

#[tokio::test(flavor = "multi_thread")]
async fn gap_during_a_bootstrap_retries_once_then_too_slow() {
    // Small frames and a history that does not compress, so a snapshot is
    // far larger than the connection holds in flight (its queue and the
    // socket buffer); each burst is larger than the ring, waits for a line
    // on stdin, and is one line so it pushes little history out.
    let server = start(Limits {
        ring: 16 * 1024,
        max_frame: 16 * 1024,
        ..fast_limits()
    });
    let mut c = client(&server).await;
    let mut ctl = client(&server).await;
    ctl.ok("subscribe", json!({})).await;
    let script = "head -c 600000 /dev/urandom | base64; echo FIRST-DONE; \
                  read go; head -c 100000 /dev/zero | tr '\\0' y; echo SECOND-DONE; \
                  read go; head -c 100000 /dev/zero | tr '\\0' y";
    let session = ctl
        .create(
            json!({"argv": ["/bin/sh", "-c", script], "on_exit": "keep", "cols": 20, "rows": 5}),
        )
        .await;
    wait_text(&mut ctl, &session.id, "FIRST-DONE").await;
    let go = json!({"session": session.id, "data": b64("go\n")});

    c.open(1, json!({"session": session.id})).await;
    let first = c.bootstrap_ready(1).await;
    // Not reading: the remainder fills the queue to the high-water mark
    // while the burst pushes the cursor out of the ring. Output (the echo
    // of `go`, the start of the burst) is forwarded until the queue fills.
    ctl.ok("write", go.clone()).await;
    wait_text(&mut ctl, &session.id, "SECOND-DONE").await;
    let mut delivered = 0u64;
    let gap = loop {
        let (header, payload) = c.recv_on(1).await;
        match header.frame_type {
            FrameType::Snapshot => {}
            FrameType::Output => delivered += parse_output(&payload).unwrap().1.len() as u64,
            FrameType::Gap => break serde_json::from_slice::<Gap>(&payload).unwrap(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(gap.from, first.attached.offset + delivered);
    let second = c.bootstrap_ready(1).await;
    assert_eq!(gap.to, second.attached.offset);
    assert!(second.attached.offset > first.attached.offset);

    // The same during the bootstrap that followed the gap is too_slow.
    ctl.ok("write", go).await;
    ctl.wait_exit(&session.id).await;
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
    ctl.wait_attachments(&session.id, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn input_overflow_keeps_the_attachment() {
    let server = start(Limits {
        input_queue: 16,
        ..fast_limits()
    });
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
async fn queries_are_answered_from_the_latest_profile_and_cell_size() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "stty raw -echo; echo READY; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "READY").await;
    c.open(
        1,
        json!({
            "session": session.id,
            "viewport": {"cols": 80, "rows": 24, "cell_width": 8, "cell_height": 16},
            "profile": {"scheme": "dark", "bg": "#102030", "fg": "#ffffff", "cursor": "#ff0000"},
        }),
    )
    .await;
    c.bootstrap(1).await;

    c.send(FrameType::Input, 1, b"\x1b[>0q").await;
    c.output_until(1, b"\x1bP>|clankermux test\x1b\\").await;
    // XTGETTCAP TN names the terminfo entry the session runs under.
    c.send(FrameType::Input, 1, b"\x1bP+q544E\x1b\\").await;
    c.output_until(1, b"\x1bP1+r544E=787465726D2D67686F73747479\x1b\\")
        .await;
    c.send(FrameType::Input, 1, b"\x1b[?996n").await;
    c.output_until(1, b"\x1b[?997;1n").await;
    c.send(FrameType::Input, 1, b"\x1b]10;?\x1b\\").await;
    c.output_until(1, b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\").await;
    c.send(FrameType::Input, 1, b"\x1b]11;?\x1b\\").await;
    c.output_until(1, b"\x1b]11;rgb:1010/2020/3030\x1b\\").await;
    c.send(FrameType::Input, 1, b"\x1b]12;?\x1b\\").await;
    c.output_until(1, b"\x1b]12;rgb:ffff/0000/0000\x1b\\").await;
    c.send(FrameType::Input, 1, b"\x1b[16t").await;
    c.output_until(1, b"\x1b[6;16;8t").await;
    // `cat` echoes the mode back to the terminal; the answer after it
    // shows it is set before the other attachment's profile arrives.
    c.send(FrameType::Input, 1, b"\x1b[?2031h\x1b[?996n").await;
    c.output_until(1, b"\x1b[?997;1n").await;

    // A second writable attachment's profile replaces it; with mode 2031
    // set the changed answer is reported unasked. Its report carries no
    // cell size, which stays.
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
    c.send(FrameType::Input, 1, b"\x1b[16t").await;
    c.output_until(1, b"\x1b[6;16;8t").await;

    // Two reports in one coalescing window: the cell size of the earlier
    // one survives the later one that omitted it.
    c.send_json(
        FrameType::Viewport,
        1,
        &json!({"cols": 81, "rows": 24, "cell_width": 9, "cell_height": 18}),
    )
    .await;
    c.send_json(FrameType::Viewport, 1, &json!({"cols": 82, "rows": 24}))
        .await;
    // Both reports have applied once the later size has.
    while c.expect_resized(1).await.cols != 82 {}
    c.send(FrameType::Input, 1, b"\x1b[16t").await;
    c.output_until(1, b"\x1b[6;18;9t").await;

    // Without a scheme the background decides.
    other
        .send_json(
            FrameType::Viewport,
            7,
            &json!({"cols": 82, "rows": 24, "profile": {"bg": "#000000"}}),
        )
        .await;
    c.output_until(1, b"\x1b[?997;1n").await;
}
