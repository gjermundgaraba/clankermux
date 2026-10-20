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

fn error_kind(err: FrameError) -> &'static str {
    match err {
        FrameError::UnknownType(_) => "unknown_type",
        FrameError::TooLong { .. } => "too_long",
        FrameError::Short => "short",
        FrameError::LengthMismatch { .. } => "length_mismatch",
    }
}

#[test]
fn frame_type_vectors() {
    let vectors: Value = serde_json::from_str(FRAMES).unwrap();
    let types = vectors["types"].as_object().unwrap();
    assert_eq!(types.len(), FrameType::ALL.len());
    for (name, value) in types {
        let value = value.as_u64().unwrap() as u8;
        let frame_type = FrameType::from_u8(value).unwrap_or_else(|| panic!("{name}"));
        // The vectors name types in snake_case; the enum in CamelCase.
        let camel: String = name
            .split('_')
            .map(|w| w[..1].to_uppercase() + &w[1..])
            .collect();
        assert_eq!(format!("{frame_type:?}"), camel);
    }
    let known: Vec<u64> = types.values().map(|v| v.as_u64().unwrap()).collect();
    for byte in 0..=u8::MAX {
        assert_eq!(
            FrameType::from_u8(byte).is_some(),
            known.contains(&u64::from(byte)),
            "{byte:#x}"
        );
    }
}

#[test]
fn frame_vectors() {
    let vectors: Value = serde_json::from_str(FRAMES).unwrap();
    let max_frame = Limits::default().max_frame;
    for v in vectors["frames"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let bytes = hex(v["hex"].as_str().unwrap());
        let mut decoder = FrameDecoder::default();
        decoder.push(&bytes);
        match v.get("error") {
            Some(expected) => {
                let err = parse_frame(&bytes, max_frame).expect_err(name);
                assert_eq!(error_kind(err), expected.as_str().unwrap(), "{name}");
                // The stream decoder sees the same header errors, and waits
                // for more bytes otherwise.
                match decoder.next_frame(max_frame) {
                    Err(stream_err) => assert_eq!(stream_err, err, "{name}"),
                    Ok(_) => assert!(
                        matches!(err, FrameError::Short | FrameError::LengthMismatch { .. }),
                        "{name}"
                    ),
                }
            }
            None => {
                let (header, payload) = parse_frame(&bytes, max_frame).expect(name);
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
                    Some((header, payload.to_vec())),
                    "{name}: stream"
                );
                assert_eq!(
                    decoder.next_frame(max_frame).unwrap(),
                    None,
                    "{name}: trailing bytes"
                );
                let encoded = encode_frame(header.frame_type, header.channel, payload);
                let mut expected = bytes.clone();
                expected[1] = 0;
                assert_eq!(encoded, expected, "{name}: re-encode with flags 0");
            }
        }
    }
}

#[test]
fn stream_decoder_handles_pieces_and_boundaries() {
    let first = encode_frame(FrameType::Output, 7, &encode_output(42, b"abc"));
    let second = encode_frame(FrameType::Close, 7, b"");
    let third = encode_frame(FrameType::Input, 7, b"xyz");
    let max_frame = u64::from(first.len() as u32 - HEADER_LEN as u32);

    let mut decoder = FrameDecoder::default();
    for byte in &first[..first.len() - 1] {
        decoder.push(&[*byte]);
        assert_eq!(decoder.next_frame(max_frame).unwrap(), None);
    }
    // Two frames and part of a third arrive together: both decode, the
    // partial one is kept after compaction, then completes.
    let mut rest = first[first.len() - 1..].to_vec();
    rest.extend_from_slice(&second);
    rest.extend_from_slice(&third[..5]);
    decoder.push(&rest);
    let (header, payload) = decoder.next_frame(max_frame).unwrap().unwrap();
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
    assert_eq!(
        decoder.next_frame(max_frame).unwrap().unwrap().0.frame_type,
        FrameType::Close
    );
    assert_eq!(decoder.next_frame(max_frame).unwrap(), None);
    decoder.push(&third[5..]);
    assert_eq!(decoder.next_frame(max_frame).unwrap().unwrap().1, b"xyz");

    // A length equal to max_frame passes; one above fails.
    assert!(parse_frame(&first, max_frame).is_ok());
    assert_eq!(
        parse_frame(&first, max_frame - 1),
        Err(FrameError::TooLong {
            length: 11,
            max_frame: max_frame - 1
        })
    );
}

fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(name: &str, json: &Value) {
    let typed: T = serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(&serde_json::to_value(&typed).unwrap(), json, "{name}");
}

#[test]
fn message_vectors() {
    let vectors: serde_json::Map<String, Value> = serde_json::from_str(MESSAGES).unwrap();
    for (name, json) in &vectors {
        let mut parts = name.split('/');
        let kind = parts.next().unwrap();
        let variant = parts.next().unwrap().split('-').next().unwrap();
        match (kind, variant) {
            ("hello", _) => round_trip::<Hello>(name, json),
            ("welcome", _) => round_trip::<Welcome>(name, json),
            ("request", _) => round_trip::<Request>(name, json),
            ("reply", _) => round_trip::<Reply>(name, json),
            ("error", _) => round_trip::<ErrorMessage>(name, json),
            ("session", _) => round_trip::<Session>(name, json),
            ("page", _) => round_trip::<Page>(name, json),
            ("read", _) => round_trip::<Read>(name, json),
            ("tail", _) => round_trip::<Tail>(name, json),
            ("subscribed", _) => round_trip::<Subscribed>(name, json),
            ("event", _) => round_trip::<Numbered>(name, json),
            ("acked", _) => round_trip::<Acked>(name, json),
            ("params", "create") => round_trip::<CreateParams>(name, json),
            ("params", "list") => round_trip::<ListParams>(name, json),
            ("params", "inspect" | "end" | "remove") => round_trip::<SessionParams>(name, json),
            ("params", "describe") => round_trip::<catalog::DescribeParams>(name, json),
            ("empty", _) => round_trip::<Empty>(name, json),
            ("params", "signal") => round_trip::<SignalParams>(name, json),
            ("params", "label") => round_trip::<LabelParams>(name, json),
            ("params", "resize") => round_trip::<ResizeParams>(name, json),
            ("params", "read") => round_trip::<ReadParams>(name, json),
            ("params", "tail") => round_trip::<TailParams>(name, json),
            ("params", "send") => round_trip::<SendParams>(name, json),
            ("params", "wait") => round_trip::<WaitParams>(name, json),
            ("params", "cancel") => round_trip::<CancelParams>(name, json),
            ("params", "detach") => round_trip::<DetachParams>(name, json),
            ("sent", _) => round_trip::<Sent>(name, json),
            ("waited", _) => round_trip::<Waited>(name, json),
            ("cancelled", _) => round_trip::<Cancelled>(name, json),
            ("params", "subscribe") => round_trip::<SubscribeParams>(name, json),
            ("params", "ack") => round_trip::<AckParams>(name, json),
            ("open", _) => round_trip::<Open>(name, json),
            ("viewport", _) => round_trip::<Viewport>(name, json),
            ("attached", _) => round_trip::<Attached>(name, json),
            ("resized", _) => round_trip::<Resized>(name, json),
            ("exited", _) => round_trip::<Exited>(name, json),
            ("gap", _) => round_trip::<Gap>(name, json),
            ("closed", _) => round_trip::<Closed>(name, json),
            _ => panic!("no type for vector {name}"),
        }
    }
}

#[test]
fn limit_defaults_match_vector() {
    let expected: Value = serde_json::from_str(LIMITS).unwrap();
    assert_eq!(serde_json::to_value(Limits::default()).unwrap(), expected);
}

#[test]
fn client_payloads_refuse_unknown_fields_and_server_payloads_ignore_them() {
    assert!(
        serde_json::from_str::<Hello>(
            r#"{"version":1,"client":{"name":"x","version":"0"},"future":true}"#
        )
        .is_err()
    );
    let hello: Hello =
        serde_json::from_str(r#"{"version":1,"client":{"name":"x","version":"0"}}"#).unwrap();
    assert_eq!(hello.capabilities, Vec::<String>::new());
    assert!(serde_json::from_str::<Request>(r#"{"id":-1,"method":"list"}"#).is_err());
    assert!(
        serde_json::from_str::<Request>(r#"{"id":-1,"method":"list","params":{},"x":0}"#).is_err()
    );
    let request: Request =
        serde_json::from_str(r#"{"id":-1,"method":"list","params":{}}"#).unwrap();
    assert_eq!(request.id, -1);
    assert!(serde_json::from_value::<ListParams>(request.params).is_ok());
    assert!(serde_json::from_str::<Viewport>(r#"{"cols":1,"rows":1,"x":0}"#).is_err());
    assert!(serde_json::from_str::<Open>(r#"{"session":"s","profile":{"x":0}}"#).is_err());
    assert!(serde_json::from_str::<ReadParams>(r#"{"session":"s","scope":"all"}"#).is_err());
    assert!(serde_json::from_str::<TailParams>(r#"{"session":"s"}"#).is_err());

    let reply: Reply = serde_json::from_str(r#"{"id":1,"result":{},"future":true}"#).unwrap();
    assert_eq!(reply.id, 1);
    let gap: Gap = serde_json::from_str(r#"{"from":1,"to":2,"future":true}"#).unwrap();
    assert_eq!(gap.to, 2);
}

fn closed_objects(schema: &Value, defs: &serde_json::Map<String, Value>, out: &mut Vec<String>) {
    let mut names = Vec::new();
    catalog::refs(schema, &mut names);
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![schema.clone()];
    while let Some(name) = names.pop() {
        if seen.insert(name.clone()) {
            let def = defs[&name].clone();
            catalog::refs(&def, &mut names);
            stack.push(def);
        }
    }
    fn walk(schema: &Value, out: &mut Vec<String>) {
        match schema {
            Value::Object(map) => {
                if map.contains_key("properties") {
                    out.push(format!(
                        "{}",
                        map.get("additionalProperties") == Some(&Value::Bool(false))
                    ));
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    stack.iter().for_each(|s| walk(s, out));
}

#[test]
fn params_schemas_are_closed_and_the_server_s_are_open() {
    let catalog = catalog::catalog("0");
    for method in &catalog.methods {
        let mut params = Vec::new();
        closed_objects(&method.params, &catalog.defs, &mut params);
        assert!(!params.is_empty(), "{}: no object", method.name);
        assert!(
            params.iter().all(|c| c == "true"),
            "{}: params open",
            method.name
        );
        let mut result = Vec::new();
        closed_objects(&method.result, &catalog.defs, &mut result);
        assert!(
            result.iter().all(|c| c == "false"),
            "{}: result closed",
            method.name
        );
    }
    for event in &catalog.events {
        let mut fields = Vec::new();
        closed_objects(&event.schema, &catalog.defs, &mut fields);
        assert!(fields.iter().all(|c| c == "false"), "{}", event.name);
    }
    for frame in &catalog.frames {
        let Some(schema) = &frame.schema else {
            continue;
        };
        let mut fields = Vec::new();
        closed_objects(schema, &catalog.defs, &mut fields);
        // `HELLO.client` and `REQUEST.params` are objects of their own.
        let closed = frame.direction == catalog::Direction::Client;
        assert!(
            fields.iter().all(|c| *c == closed.to_string()),
            "{}: {fields:?}",
            frame.name
        );
    }
}

#[test]
fn every_method_has_typed_examples_and_known_errors() {
    let catalog = catalog::catalog("0");
    let typed = catalog::typed();
    assert_eq!(typed.len(), catalog.methods.len());
    let codes: Vec<ErrorCode> = catalog.errors.iter().map(|e| e.code).collect();
    for (method, (name, check)) in catalog.methods.iter().zip(typed) {
        assert_eq!(method.name, name);
        assert!(!method.examples.is_empty(), "{name}: no example");
        for example in &method.examples {
            check(&example.params, &example.result).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert!(!method.summary.is_empty(), "{name}");
        assert!(method.errors.contains(&ErrorCode::InvalidRequest), "{name}");
        assert!(method.errors.iter().all(|e| codes.contains(e)), "{name}");
    }
    let names: std::collections::BTreeSet<&str> =
        catalog.methods.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names.len(), catalog.methods.len(), "names are unique");
    // Every code, each once, with its meaning.
    assert_eq!(codes.len(), 17);
    assert_eq!(
        codes
            .iter()
            .map(|c| c.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        codes.len()
    );
    assert!(catalog.errors.iter().all(|e| !e.summary.is_empty()));
    assert!(catalog.events.iter().all(|e| !e.summary.is_empty()));
    assert_eq!(catalog.frames.len(), FrameType::ALL.len());
    assert!(
        catalog
            .frames
            .iter()
            .all(|f| f.schema.is_some() == (f.payload == catalog::Payload::Json))
    );
}

#[test]
fn every_reference_resolves_and_nothing_is_null() {
    let catalog = catalog::catalog("0");
    let json = serde_json::to_value(&catalog).unwrap();
    let mut names = Vec::new();
    catalog::refs(&json, &mut names);
    assert!(!names.is_empty());
    for name in names {
        assert!(catalog.defs.contains_key(&name), "{name}");
    }
    let text = serde_json::to_string(&json).unwrap();
    assert!(!text.contains(r#""null""#), "a schema allows null");
    // Well inside a reply.
    assert!(
        (text.len() as u64) < Limits::default().max_frame / 4,
        "{}",
        text.len()
    );
}

#[test]
fn describe_one_method_carries_what_it_refers_to() {
    let catalog = catalog::catalog("1.2.3");
    for method in &catalog.methods {
        let one = catalog.describe(&method.name).unwrap();
        assert_eq!((one.protocol, one.version.as_str()), (VERSION, "1.2.3"));
        assert_eq!(&one.method, method);
        // The examples are values, not schemas: `describe`'s holds a
        // Description with `$defs` of its own.
        let mut names = Vec::new();
        catalog::refs(&one.method.params, &mut names);
        catalog::refs(&one.method.result, &mut names);
        catalog::refs(&serde_json::to_value(&one.defs).unwrap(), &mut names);
        for name in names {
            assert!(one.defs.contains_key(&name), "{}: {name}", method.name);
        }
        assert!(one.defs.len() < catalog.defs.len(), "{}", method.name);
    }
    assert!(catalog.describe("write").is_none());
    // Both answers read back as `Described`, each as its own shape.
    let whole: catalog::Described =
        serde_json::from_value(serde_json::to_value(&catalog).unwrap()).unwrap();
    assert!(matches!(whole, catalog::Described::Catalog(c) if c == catalog));
    let one = catalog.describe("send").unwrap();
    let back: catalog::Described =
        serde_json::from_value(serde_json::to_value(&one).unwrap()).unwrap();
    assert!(matches!(back, catalog::Described::Method(d) if d == one));
}

#[test]
fn protocol_md_is_rendered_from_the_catalog() {
    let doc = include_str!("../../../PROTOCOL.md");
    let rendered = render::splice(doc, &catalog::catalog("")).unwrap();
    assert!(
        rendered == doc,
        "PROTOCOL.md is stale: run `cargo xtask protocol`"
    );
}

#[test]
fn splice_renders_regions_and_keeps_the_rest() {
    let catalog = catalog::catalog("");
    let doc = "Prose.\n<!-- generated: type Cancelled -->\nold\n<!-- end generated -->\nMore.\n";
    let once = render::splice(doc, &catalog).unwrap();
    assert_eq!(
        once,
        "Prose.\n<!-- generated: type Cancelled -->\n\
         | Field       | Type | Meaning |\n\
         | ----------- | ---- | ------- |\n\
         | `cancelled` | bool | |\n\
         <!-- end generated -->\nMore.\n"
    );
    assert_eq!(render::splice(&once, &catalog).unwrap(), once);
    // A mark inside a line is prose that shows one.
    let quoted = "Write `<!-- generated: methods -->` and `<!-- end generated -->`.\n";
    assert_eq!(render::splice(quoted, &catalog).unwrap(), quoted);
    for bad in [
        "<!-- generated: methods -->\nno end\n",
        "stray\n<!-- end generated -->\n",
        "<!-- generated: nothing -->\n<!-- end generated -->\n",
        "<!-- generated: type Nothing -->\n<!-- end generated -->\n",
        "<!-- generated: methods -->\n<!-- generated: events -->\n<!-- end generated -->\n",
    ] {
        assert!(render::splice(bad, &catalog).is_err(), "{bad}");
    }
}

#[test]
fn one_method_renders_with_the_types_it_uses() {
    let catalog = catalog::catalog("");
    let send = render::method(&catalog.describe("send").unwrap());
    for part in [
        "# send\n",
        "(mutation)",
        "| `wait`    | {until, timeout_ms?}?",
        "exactly one of `text`, `keys` and `data`",
        "## Result: Sent",
        "## Waited",
        "## SendWait",
        "## Until",
        "## OutputMatch",
        "`input_overflow`",
        r#"{"method": "send", "params": {"session":"#,
    ] {
        assert!(send.contains(part), "{part:?} in\n{send}");
    }
    let end = render::method(&catalog.describe("end").unwrap());
    assert!(end.contains("## Result: `{}`"), "{end}");
}
