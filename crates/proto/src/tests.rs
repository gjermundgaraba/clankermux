use serde_json::Value;

use crate::*;

const FRAMES: &str = include_str!("../vectors/frames.json");
const MESSAGES: &str = include_str!("../vectors/messages.json");
const LIMITS: &str = include_str!("../vectors/limits.json");

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn frame_vectors() {
    let vectors: Vec<Value> = serde_json::from_str(FRAMES).unwrap();
    let max_frame = Limits::default().max_frame as u32;
    for v in &vectors {
        let name = v["name"].as_str().unwrap();
        let bytes = hex(v["hex"].as_str().unwrap());
        let mut decoder = FrameDecoder::default();
        decoder.push(&bytes);
        match v.get("error") {
            Some(expected) => {
                let err = decoder.next_frame(max_frame).expect_err(name);
                assert_eq!(err.to_string(), expected.as_str().unwrap(), "{name}");
            }
            None => {
                let (header, payload) = decoder.next_frame(max_frame).unwrap().expect(name);
                assert_eq!(
                    header.frame_type as u8,
                    v["type"].as_u64().unwrap() as u8,
                    "{name}"
                );
                assert_eq!(
                    header.channel,
                    v["channel"].as_u64().unwrap() as u32,
                    "{name}"
                );
                assert_eq!(payload, hex(v["payload"].as_str().unwrap()), "{name}");
                assert_eq!(
                    decoder.next_frame(max_frame).unwrap(),
                    None,
                    "{name}: trailing bytes"
                );
                let encoded = encode_frame(header.frame_type, header.channel, &payload);
                let mut expected = bytes.clone();
                expected[1] = 0;
                assert_eq!(encoded, expected, "{name}: re-encode with flags 0");
            }
        }
    }
}

#[test]
fn frames_arrive_in_pieces() {
    let bytes = encode_frame(FrameType::Output, 7, &encode_output(42, b"abc"));
    let mut decoder = FrameDecoder::default();
    for byte in &bytes[..bytes.len() - 1] {
        decoder.push(&[*byte]);
        assert_eq!(decoder.next_frame(1024).unwrap(), None);
    }
    decoder.push(&bytes[bytes.len() - 1..]);
    let (header, payload) = decoder.next_frame(1024).unwrap().unwrap();
    assert_eq!(
        header,
        Header {
            frame_type: FrameType::Output,
            channel: 7,
            length: 11
        }
    );
    assert_eq!(parse_output(&payload), Some((42, &b"abc"[..])));
    assert_eq!(parse_output(&payload[..7]), None);
}

fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(name: &str, json: &Value) {
    let typed: T = serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(&serde_json::to_value(&typed).unwrap(), json, "{name}");
}

#[test]
fn message_vectors() {
    let vectors: serde_json::Map<String, Value> = serde_json::from_str(MESSAGES).unwrap();
    for (name, json) in &vectors {
        match name.split('/').next().unwrap() {
            "hello" => round_trip::<Hello>(name, json),
            "welcome" => round_trip::<Welcome>(name, json),
            "request" => round_trip::<Request>(name, json),
            "reply" => round_trip::<Reply>(name, json),
            "error" => round_trip::<ErrorMessage>(name, json),
            "session" => round_trip::<Session>(name, json),
            "page" => round_trip::<Page>(name, json),
            "read" => round_trip::<Read>(name, json),
            "event" => round_trip::<Event>(name, json),
            "create" => round_trip::<CreateParams>(name, json),
            "open" => round_trip::<Open>(name, json),
            "viewport" => round_trip::<Viewport>(name, json),
            "attached" => round_trip::<Attached>(name, json),
            "resized" => round_trip::<Resized>(name, json),
            "exited" => round_trip::<Exited>(name, json),
            "gap" => round_trip::<Gap>(name, json),
            "closed" => round_trip::<Closed>(name, json),
            other => panic!("no type for vector {other}"),
        }
    }
}

#[test]
fn limit_defaults_match_vector() {
    let expected: Value = serde_json::from_str(LIMITS).unwrap();
    assert_eq!(serde_json::to_value(Limits::default()).unwrap(), expected);
}

#[test]
fn unknown_fields_are_ignored_and_errors_named() {
    let hello: Hello =
        serde_json::from_str(r#"{"version":1,"client":{"name":"x","version":"0"},"future":true}"#)
            .unwrap();
    assert_eq!(hello.capabilities, Vec::<String>::new());
    assert_eq!(
        serde_json::to_string(&ErrorCode::NoSuchSession).unwrap(),
        r#""no_such_session""#
    );
    assert_eq!(FrameType::from_u8(0x98), None);
    assert!(FrameType::Error.is_control() && FrameType::Error.is_attachment());
    assert!(!FrameType::Open.is_control() && FrameType::Open.is_json());
}
