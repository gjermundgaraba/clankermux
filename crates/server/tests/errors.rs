//! Errors conformance: the placement rule that an attachment-channel
//! error ends the attachment and frees the channel. The codes
//! themselves, request errors carrying their id, and id-less connection
//! errors before a close are raised across the other suites (see the
//! ledger's M7 table). `signal_failed` needs a running session whose
//! process group is empty and whose PTY nobody holds, which cannot exist
//! (an unheld PTY ends the stream), so it is not exercised.

mod common;

use common::*;
use proto::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn an_open_error_frees_the_channel() {
    let server = start(fast_limits()).await;
    let mut c = client(&server).await;
    let session = c
        .create(json!({"argv": ["/bin/sh", "-c", "echo up; exec cat"]}))
        .await;
    wait_text(&mut c, &session.id, "up").await;
    for (open, code) in [
        (json!({"session": "nope"}), ErrorCode::NoSuchSession),
        (
            json!({"session": session.id, "viewport": {"cols": 0, "rows": 1}}),
            ErrorCode::InvalidRequest,
        ),
    ] {
        c.open(7, open).await;
        assert_eq!(c.expect_error(7).await, code);
        c.wait_attachments(&session.id, 0).await;
        // The channel is free again right after the error.
        c.open(7, json!({"session": session.id})).await;
        c.bootstrap(7).await;
        c.send(FrameType::Close, 7, &[]).await;
        assert_eq!(c.expect_closed(7).await, ClosedReason::Closed);
        c.wait_attachments(&session.id, 0).await;
    }
}
