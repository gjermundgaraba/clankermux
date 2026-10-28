use serde_json::Value;

use crate::*;

const FRAMES: &str = include_str!("../vectors/frames.json");
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
        let value = u8::try_from(value.as_u64().unwrap()).unwrap();
        let frame_type = FrameType::from_u8(value).unwrap_or_else(|| panic!("{name}"));
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
        if let Some(expected) = v.get("error") {
            let err = parse_frame(&bytes, max_frame).expect_err(name);
            assert_eq!(error_kind(err), expected.as_str().unwrap(), "{name}");
            match decoder.next_frame(max_frame) {
                Err(stream_err) => assert_eq!(stream_err, err, "{name}"),
                Ok(_) => assert!(
                    matches!(err, FrameError::Short | FrameError::LengthMismatch { .. }),
                    "{name}"
                ),
            }
        } else {
            let (header, payload) = parse_frame(&bytes, max_frame).expect(name);
            assert_eq!(
                header.frame_type as u8,
                u8::try_from(v["type"].as_u64().unwrap()).unwrap(),
                "{name}"
            );
            assert_eq!(
                header.channel,
                u32::try_from(v["channel"].as_u64().unwrap()).unwrap(),
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

#[test]
fn stream_decoder_handles_pieces_and_boundaries() {
    let first = encode_frame(FrameType::Output, 7, &encode_output(42, b"abc"));
    let second = encode_frame(FrameType::Close, 7, b"");
    let third = encode_frame(FrameType::Input, 7, b"xyz");
    let max_frame = u64::try_from(first.len() - HEADER_LEN).unwrap();

    let mut decoder = FrameDecoder::default();
    for byte in &first[..first.len() - 1] {
        decoder.push(&[*byte]);
        assert_eq!(decoder.next_frame(max_frame).unwrap(), None);
    }
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

    assert!(parse_frame(&first, max_frame).is_ok());
    assert_eq!(
        parse_frame(&first, max_frame - 1),
        Err(FrameError::TooLong {
            length: 11,
            max_frame: max_frame - 1
        })
    );
}

pub(crate) type Check = fn(&Value) -> Result<(), String>;

pub(crate) fn same<T: serde::Serialize + serde::de::DeserializeOwned>(
    value: &Value,
) -> Result<(), String> {
    let typed: T = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let back = serde_json::to_value(typed).map_err(|e| e.to_string())?;
    (&back == value)
        .then_some(())
        .ok_or_else(|| format!("{value} came back as {back}"))
}

#[test]
fn message_vectors() {
    let vectors: serde_json::Map<String, Value> = serde_json::from_str(catalog::VECTORS).unwrap();
    let typed = catalog::typed();
    for (name, json) in &vectors {
        let mut parts = name.split('/');
        let kind = parts.next().unwrap();
        let variant = parts.next().unwrap().split('-').next().unwrap();
        match (kind, variant) {
            ("params", method) => {
                let (_, params, _) = typed
                    .iter()
                    .find(|(name, ..)| *name == method)
                    .unwrap_or_else(|| panic!("no method for vector {name}"));
                params(json)
            }
            ("hello", _) => same::<Hello>(json),
            ("welcome", _) => same::<Welcome>(json),
            ("request", _) => same::<Request>(json),
            ("reply", _) => same::<Reply>(json),
            ("error", _) => same::<ErrorMessage>(json),
            ("session", _) => same::<Session>(json),
            ("page", _) => same::<Page>(json),
            ("read", _) => same::<Read>(json),
            ("tail", _) => same::<Tail>(json),
            ("subscribed", _) => same::<Subscribed>(json),
            ("event", _) => same::<Numbered>(json),
            ("acked", _) => same::<Acked>(json),
            ("empty", _) => same::<Empty>(json),
            ("sent", _) => same::<Sent>(json),
            ("waited", _) => same::<Waited>(json),
            ("cancelled", _) => same::<Cancelled>(json),
            ("open", _) => same::<Open>(json),
            ("viewport", _) => same::<Viewport>(json),
            ("attached", _) => same::<Attached>(json),
            ("resized", _) => same::<Resized>(json),
            ("exited", _) => same::<Exited>(json),
            ("gap", _) => same::<Gap>(json),
            ("closed", _) => same::<Closed>(json),
            _ => panic!("no type for vector {name}"),
        }
        .unwrap_or_else(|e| panic!("{name}: {e}"));
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

fn closed_objects(schema: &Value, defs: &serde_json::Map<String, Value>, out: &mut Vec<bool>) {
    fn walk(schema: &Value, out: &mut Vec<bool>) {
        match schema {
            Value::Object(map) => {
                if map.contains_key("properties") {
                    out.push(map.get("additionalProperties") == Some(&Value::Bool(false)));
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }

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
    for s in &stack {
        walk(s, out);
    }
}

#[test]
fn params_schemas_are_closed_and_the_server_s_are_open() {
    let catalog = catalog::catalog(Some("0"));
    for method in &catalog.methods {
        let mut params = Vec::new();
        closed_objects(&method.params, &catalog.defs, &mut params);
        assert!(!params.is_empty(), "{}: no object", method.name);
        assert!(params.iter().all(|&c| c), "{}: params open", method.name);
        let mut result = Vec::new();
        closed_objects(&method.result, &catalog.defs, &mut result);
        assert!(result.iter().all(|&c| !c), "{}: result closed", method.name);
    }
    for event in &catalog.events {
        let mut fields = Vec::new();
        closed_objects(&event.schema, &catalog.defs, &mut fields);
        assert!(fields.iter().all(|&c| !c), "{}", event.name);
    }
    for frame in &catalog.frames {
        let Some(schema) = &frame.schema else {
            continue;
        };
        let mut fields = Vec::new();
        closed_objects(schema, &catalog.defs, &mut fields);
        let closed = frame.direction == catalog::Direction::Client;
        assert!(
            fields.iter().all(|&c| c == closed),
            "{}: {fields:?}",
            frame.name
        );
    }
}

#[test]
fn every_method_s_examples_are_its_types() {
    let catalog = catalog::catalog(Some("0"));
    for (name, params, result) in catalog::typed() {
        let method = catalog.method(name).unwrap();
        assert!(!method.examples.is_empty(), "{name}: no example");
        for example in &method.examples {
            params(&example.params)
                .and_then(|()| result(&example.result))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
}

#[test]
fn method_names_are_unique() {
    let catalog = catalog::catalog(Some("0"));
    let names: std::collections::BTreeSet<&str> =
        catalog.methods.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names.len(), catalog.methods.len());
}

#[test]
fn every_method_can_refuse_its_params() {
    for method in catalog::catalog(Some("0")).methods {
        assert!(
            method.errors.contains(&ErrorCode::InvalidRequest),
            "{}",
            method.name
        );
    }
}

#[test]
fn every_method_event_and_error_code_says_what_it_does() {
    let catalog = catalog::catalog(Some("0"));
    let summaries = catalog
        .methods
        .iter()
        .map(|m| (m.name.clone(), &m.summary))
        .chain(catalog.events.iter().map(|e| (e.name.clone(), &e.summary)))
        .chain(
            catalog
                .errors
                .iter()
                .map(|e| (e.code.to_string(), &e.summary)),
        );
    for (name, summary) in summaries {
        assert!(!summary.is_empty(), "{name}");
    }
}

#[test]
fn a_frame_has_a_schema_exactly_when_its_payload_is_json() {
    for frame in catalog::catalog(Some("0")).frames {
        assert_eq!(
            frame.schema.is_some(),
            frame.payload == catalog::Payload::Json,
            "{}",
            frame.name
        );
    }
}

#[test]
fn references_and_definitions_match_and_nothing_is_null() {
    let catalog = catalog::catalog(Some("0"));
    let json = serde_json::to_value(&catalog).unwrap();
    let mut names = std::collections::BTreeSet::new();
    catalog::refs(&json, &mut names);
    let defs: std::collections::BTreeSet<_> = catalog.defs.keys().cloned().collect();
    assert_eq!(
        names, defs,
        "every reference resolves, every definition is referenced"
    );
    let text = serde_json::to_string(&json).unwrap();
    assert!(!text.contains(r#""null""#), "a schema allows null");
    assert!(
        (text.len() as u64) < Limits::default().max_frame / 4,
        "{}",
        text.len()
    );
}

#[test]
fn describe_one_method_carries_what_it_refers_to() {
    let catalog = catalog::catalog(Some("1.2.3"));
    for method in &catalog.methods {
        let one = catalog.describe(&method.name).unwrap();
        assert_eq!((one.protocol, one.version.as_str()), (VERSION, "1.2.3"));
        assert_eq!(&one.method, method);
        let mut names = Vec::new();
        catalog::refs(&one.method.params, &mut names);
        catalog::refs(&one.method.result, &mut names);
        catalog::refs(&serde_json::to_value(&one.defs).unwrap(), &mut names);
        for name in names {
            assert!(one.defs.contains_key(&name), "{}: {name}", method.name);
        }
        assert!(one.defs.len() < catalog.defs.len(), "{}", method.name);
    }
    assert!(catalog.describe("no_such_method").is_none());
    let whole: catalog::Described =
        serde_json::from_value(serde_json::to_value(&catalog).unwrap()).unwrap();
    assert!(matches!(whole, catalog::Described::Catalog(c) if c == catalog));
    let one = catalog.describe("send").unwrap();
    let back: catalog::Described =
        serde_json::from_value(serde_json::to_value(&one).unwrap()).unwrap();
    assert!(matches!(back, catalog::Described::Method(d) if d == one));
}

#[test]
fn splice_renders_regions_and_keeps_the_rest() {
    let catalog = catalog::catalog(None);
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
    let catalog = catalog::catalog(None);
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

#[test]
fn output_frame_from_slices_is_one_complete_protocol_frame() {
    for (first, second) in [
        (&b"abc"[..], &b"def"[..]),
        (b"", b"xyz"),
        (b"x", b""),
        (b"", b""),
    ] {
        let frame = encode_output_frame(7, 42, first, second);
        let (header, payload) = parse_frame(&frame, 1024).unwrap();
        assert_eq!(header.frame_type, FrameType::Output);
        assert_eq!(header.channel, 7);
        assert_eq!(
            parse_output(payload),
            Some((42, [first, second].concat().as_slice()))
        );
        assert_eq!(
            frame,
            encode_frame(
                FrameType::Output,
                7,
                &encode_output(42, &[first, second].concat())
            )
        );
    }
}
