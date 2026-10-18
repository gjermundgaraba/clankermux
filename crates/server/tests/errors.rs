//! Errors conformance: the placement rules of PROTOCOL.md's Errors
//! section. The codes themselves are raised across the other suites;
//! `signal_failed` needs a running session whose process group is empty
//! and whose PTY nobody holds, which cannot exist (an unheld PTY ends
//! the stream), so it is not exercised.

mod common;

use common::*;
use proto::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn request_errors_carry_the_id_and_keep_the_connection() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    c.send_json(
        FrameType::Request,
        0,
        &json!({"id": 42, "method": "inspect", "params": {"session": "nope"}}),
    )
    .await;
    let (header, body) = c.recv_json().await;
    assert_eq!((header.frame_type, header.channel), (FrameType::Error, 0));
    assert_eq!(body["id"], 42);
    assert_eq!(body["code"], "no_such_session");
    assert!(body["message"].is_string());
    c.ok("list", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_errors_have_no_id_and_close() {
    let server = start(fast_limits()).await;
    // An attachment type on channel 0.
    let mut c = client(&server).await;
    c.send(FrameType::Input, 0, b"x").await;
    let (header, body) = c.recv_json().await;
    assert_eq!((header.frame_type, header.channel), (FrameType::Error, 0));
    assert_eq!(body["code"], "protocol_error");
    assert!(body.get("id").is_none());
    assert!(c.recv().await.is_none());
    // The server is unharmed.
    client(&server).await.ok("list", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_errors_end_the_attachment_except_three() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "echo up; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "up").await;

    // Ending: the channel is free again right after the error.
    for (open, code) in [
        (json!({"session": "nope"}), ErrorCode::NoSuchSession),
        (
            json!({"session": session.id, "viewport": {"cols": 0, "rows": 1}}),
            ErrorCode::InvalidRequest,
        ),
    ] {
        c.open(7, open).await;
        assert_eq!(c.expect_error(7).await, code);
        c.open(7, json!({"session": session.id})).await;
        c.bootstrap(7).await;
        c.send(FrameType::Close, 7, &[]).await;
        assert_eq!(c.expect_closed(7).await, ClosedReason::Closed);
        c.wait_attachments(&session.id, 0).await;
    }

    // Keeping: read_only, then not_running and input_overflow.
    c.open(8, json!({"session": session.id, "read_only": true}))
        .await;
    c.bootstrap(8).await;
    c.send(FrameType::Input, 8, b"x").await;
    assert_eq!(c.expect_error(8).await, ErrorCode::ReadOnly);
    c.send_json(FrameType::Viewport, 8, &json!({"cols": 10, "rows": 5}))
        .await;
    assert_eq!(c.expect_error(8).await, ErrorCode::ReadOnly);
    c.send(FrameType::Close, 8, &[]).await;
    assert_eq!(c.expect_closed(8).await, ClosedReason::Closed);

    let small = start(Limits {
        input_queue: 4,
        ..fast_limits()
    })
    .await;
    let mut c = client(&small).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "echo up; sleep 30"]}))
        .await;
    wait_text(&mut c, &session.id, "up").await;
    c.open(9, json!({"session": session.id})).await;
    c.bootstrap(9).await;
    c.send(FrameType::Input, 9, b"too much").await;
    assert_eq!(c.expect_error(9).await, ErrorCode::InputOverflow);
    c.ok("signal", json!({"session": session.id, "signal": "KILL"}))
        .await;
    loop {
        let (header, _) = c.recv_on(9).await;
        if header.frame_type == FrameType::Exited {
            break;
        }
    }
    c.send(FrameType::Input, 9, b"x").await;
    assert_eq!(c.expect_error(9).await, ErrorCode::NotRunning);
    c.send(FrameType::Close, 9, &[]).await;
    assert_eq!(c.expect_closed(9).await, ClosedReason::Closed);
}
