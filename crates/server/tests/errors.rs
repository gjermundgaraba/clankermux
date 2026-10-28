use proto::*;
use serde_json::json;
use testkit::*;
use tokio::io::AsyncWriteExt;

#[tokio::test(flavor = "multi_thread")]
async fn an_open_error_frees_the_channel() {
    let server = start(fast_limits());
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
        c.open(7, json!({"session": session.id})).await;
        c.bootstrap(7).await;
        c.send(FrameType::Close, 7, &[]).await;
        assert_eq!(c.expect_closed(7).await, ClosedReason::Closed);
        c.wait_attachments(&session.id, 0).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_follows_an_id_less_error() {
    let server = start(fast_limits());
    let session = {
        let mut c = client(&server).await;
        c.create(json!({"argv": ["/bin/sh", "-c", "echo up; exec cat"]}))
            .await
    };
    for _ in 0..20 {
        let mut c = client(&server).await;
        let mut bytes = encode_frame(
            FrameType::Open,
            1,
            &serde_json::to_vec(&json!({"session": session.id})).unwrap(),
        );
        bytes.extend(encode_frame(FrameType::Input, 0, b"x"));
        c.stream.write_all(&bytes).await.unwrap();
        c.stream.flush().await.unwrap();
        let mut closing = false;
        while let Some((header, payload)) = c.recv().await {
            assert!(!closing, "{header:?} after the id-less ERROR");
            if header.frame_type == FrameType::Error && header.channel == 0 {
                let error: ErrorMessage = serde_json::from_slice(&payload).unwrap();
                assert_eq!((error.id, error.code), (None, ErrorCode::ProtocolError));
                closing = true;
            }
        }
        assert!(closing, "no id-less ERROR before the close");
    }
}
