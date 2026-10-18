//! Framing fuzz against a live server: after any sequence of well-framed
//! or garbage frames and the end of input, the server answers the trailing
//! request when the bytes framed it, closes after an id-less `ERROR` when
//! they broke the stream, or closes quietly at the end of input (garbage
//! can form a valid header the rest of the input does not fill); it never
//! hangs, and it serves the next connection. The connection is past
//! `HELLO`; `OPEN` on `@session` reaches a live session.

mod common;

use common::*;
use proptest::prelude::*;
use proto::*;
use serde_json::json;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
enum Piece {
    Garbage(Vec<u8>),
    Frame {
        frame_type: FrameType,
        channel: u32,
        payload: Vec<u8>,
    },
}

fn payload(frame_type: FrameType) -> BoxedStrategy<Vec<u8>> {
    let shaped = match frame_type {
        FrameType::Request => Some(prop::sample::select(vec![
            json!({"id": 1, "method": "list", "params": {}}),
            json!({"id": 2, "method": "create", "params": {"argv": ["/bin/cat"]}}),
            json!({"id": 3, "method": "nope", "params": {}}),
            json!({"id": "x", "method": "list"}),
            json!({"method": "list", "params": {}}),
        ])),
        FrameType::Open => Some(prop::sample::select(vec![
            json!({"session": "@session"}),
            json!({"session": "@session", "viewport": {"cols": 20, "rows": 5}}),
            json!({"session": "nope"}),
            json!({"session": "@session", "resume": {"incarnation": "i", "offset": 0, "marker": 0}}),
            json!({}),
        ])),
        FrameType::Viewport => Some(prop::sample::select(vec![
            json!({"cols": 1, "rows": 1}),
            json!({"cols": 0, "rows": 0}),
        ])),
        _ => None,
    };
    match shaped {
        Some(values) => prop_oneof![
            values.prop_map(|v| serde_json::to_vec(&v).unwrap()),
            prop::collection::vec(any::<u8>(), 0..200),
        ]
        .boxed(),
        None => prop::collection::vec(any::<u8>(), 0..200).boxed(),
    }
}

fn piece() -> impl Strategy<Value = Piece> {
    prop_oneof![
        1 => prop::collection::vec(any::<u8>(), 1..40).prop_map(Piece::Garbage),
        6 => prop::sample::select(FrameType::ALL.to_vec()).prop_flat_map(|frame_type| {
            (
                prop_oneof![4 => Just(0u32), 4 => Just(1u32), 1 => any::<u32>()],
                payload(frame_type),
            )
                .prop_map(move |(channel, payload)| Piece::Frame {
                    frame_type,
                    channel,
                    payload,
                })
        }),
    ]
}

fn cases() -> u32 {
    std::env::var("CLANKERMUX_FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(160)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]

    #[test]
    fn the_server_answers_or_closes_cleanly(pieces in prop::collection::vec(piece(), 1..12)) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let server = start(fast_limits());
            let mut c = client(&server).await;
            let session = c.create(json!({"argv": ["/bin/cat"]})).await;
            let mut bytes = Vec::new();
            for piece in &pieces {
                match piece {
                    Piece::Garbage(raw) => bytes.extend(raw),
                    Piece::Frame { frame_type, channel, payload } => {
                        let payload = match std::str::from_utf8(payload) {
                            Ok(text) => text.replace("@session", &session.id).into_bytes(),
                            Err(_) => payload.clone(),
                        };
                        bytes.extend(encode_frame(*frame_type, *channel, &payload));
                    }
                }
            }
            let trailing = encode_frame(
                FrameType::Request,
                0,
                &serde_json::to_vec(&json!({"id": 999, "method": "list", "params": {}})).unwrap(),
            );
            bytes.extend(&trailing);
            // What the server will make of the bytes: the trailing request
            // is a frame of its own only if everything before it framed.
            let mut decoder = FrameDecoder::default();
            decoder.push(&bytes);
            let mut consumed = 0;
            let mut broke = false;
            loop {
                match decoder.next_frame(Limits::default().max_frame) {
                    Ok(Some((header, _))) => consumed += HEADER_LEN + header.length as usize,
                    Ok(None) => break,
                    Err(_) => {
                        broke = true;
                        break;
                    }
                }
            }
            let framed = !broke && consumed == bytes.len();
            // The server may close before everything is written. The end
            // of input lets it close rather than wait for a frame the
            // garbage promised.
            let _ = c.stream.write_all(&bytes).await;
            let _ = c.stream.shutdown().await;
            // An id-less ERROR on channel 0 is the last frame before the
            // close; a request error carries its id.
            let mut closing = false;
            let mut answered = false;
            loop {
                match c.recv().await {
                    None => break,
                    Some((header, payload)) => {
                        prop_assert!(!closing, "{:?} after an id-less ERROR", header);
                        if header.frame_type == FrameType::Error && header.channel == 0 {
                            let error: ErrorMessage = serde_json::from_slice(&payload).unwrap();
                            if error.id.is_none() {
                                prop_assert!(
                                    matches!(error.code, ErrorCode::ProtocolError | ErrorCode::InvalidRequest),
                                    "{:?}",
                                    error
                                );
                                closing = true;
                            }
                        }
                        if header.frame_type == FrameType::Reply {
                            let reply: Reply = serde_json::from_slice(&payload).unwrap();
                            if reply.id == 999 {
                                answered = true;
                            }
                        }
                    }
                }
            }
            if framed {
                prop_assert!(answered || closing, "a framed request got no answer");
            }
            // The server lives on.
            client(&server).await.ok("list", json!({})).await;
            Ok(())
        })?;
    }
}
