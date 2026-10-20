//! The catalog: every method, event, frame and error code, with JSON
//! Schemas derived from the types of this crate. `describe` answers it,
//! and PROTOCOL.md's tables are rendered from it (see `render`).

use std::collections::{BTreeMap, BTreeSet};

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::*;

/// The shared message vectors; the examples come from them.
pub const VECTORS: &str = include_str!("../vectors/messages.json");

/// What a method does to the server's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Answers from the server's state and changes nothing.
    Read,
    /// Changes a session, an attachment or a pending request.
    Mutation,
    /// Starts frames that follow the reply on the connection.
    Stream,
}

crate::message::wire_display!(Class, Direction, Payload);

/// `describe`: one method, or the whole catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeParams {
    /// A method's name; absent: every method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

/// The `describe {}` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Catalog {
    /// The protocol version, as in `WELCOME`.
    pub protocol: u32,
    /// The server's release.
    pub version: String,
    pub methods: Vec<MethodInfo>,
    pub events: Vec<EventInfo>,
    pub frames: Vec<FrameInfo>,
    pub errors: Vec<ErrorInfo>,
    /// The schemas the others refer to, as `#/$defs/NAME`.
    #[serde(rename = "$defs")]
    pub defs: Map<String, Value>,
}

/// The `describe {method}` result: the method and the schemas it refers
/// to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Description {
    /// The protocol version, as in `WELCOME`.
    pub protocol: u32,
    /// The server's release.
    pub version: String,
    pub method: MethodInfo,
    /// The schemas `method` refers to, as `#/$defs/NAME`.
    #[serde(rename = "$defs")]
    pub defs: Map<String, Value>,
}

/// The `describe` result: `Description` with `method`, else `Catalog`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Described {
    Method(Description),
    Catalog(Catalog),
}

/// One method of `REQUEST`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MethodInfo {
    pub name: String,
    pub class: Class,
    pub summary: String,
    /// JSON Schema of `params`; objects refuse undefined fields.
    pub params: Value,
    /// JSON Schema of `REPLY.result`; objects may gain fields.
    pub result: Value,
    /// The codes its `ERROR` can carry.
    pub errors: Vec<ErrorCode>,
    pub examples: Vec<Example>,
}

/// A request's `params` and the `result` of its reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Example {
    pub params: Value,
    pub result: Value,
}

/// One kind of `EVENT`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventInfo {
    /// The `event` field.
    pub name: String,
    pub summary: String,
    /// JSON Schema of the event, without `seq`.
    pub schema: Value,
    /// `EVENT` payloads, with `seq`.
    pub examples: Vec<Value>,
}

/// Which side sends a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Client,
    Server,
}

/// What a frame's payload holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Payload {
    Json,
    Raw,
    Empty,
}

/// One frame type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FrameInfo {
    pub name: String,
    /// The header's `type` byte.
    #[serde(rename = "type")]
    pub frame_type: u8,
    pub direction: Direction,
    /// `0` for control, `n` for an attachment, `0 or n` for either.
    pub channel: String,
    pub payload: Payload,
    pub summary: String,
    /// JSON Schema of a JSON payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// One error code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    pub summary: String,
}

/// One method as declared below: its types stand for its schemas.
struct Spec {
    name: &'static str,
    class: Class,
    params: fn(&mut SchemaGenerator) -> schemars::Schema,
    result: fn(&mut SchemaGenerator) -> schemars::Schema,
    errors: &'static [ErrorCode],
    /// Vector names: params, then result.
    examples: &'static [(&'static str, &'static str)],
    summary: &'static str,
}

/// Declares the methods, in the order the catalog lists them.
macro_rules! methods {
    ($(
        $name:literal $class:ident ($params:ty) -> $result:ty,
        errors [$($error:ident),*],
        examples [$(($p:literal, $r:literal)),*],
        $summary:literal;
    )*) => {
        fn specs() -> Vec<Spec> {
            vec![$(Spec {
                name: $name,
                class: Class::$class,
                params: |g| g.subschema_for::<$params>(),
                result: |g| g.subschema_for::<$result>(),
                errors: &[$(ErrorCode::$error),*],
                examples: &[$(($p, $r)),*],
                summary: $summary,
            }),*]
        }

        /// Per method, whether a params value and a result value are its
        /// types, unchanged by a round trip.
        #[cfg(test)]
        pub(crate) fn typed() -> Vec<(&'static str, fn(&Value, &Value) -> Result<(), String>)> {
            vec![$(($name, |params, result| {
                same::<$params>(params)?;
                same::<$result>(result)
            })),*]
        }
    };
}

#[cfg(test)]
fn same<T: Serialize + serde::de::DeserializeOwned>(value: &Value) -> Result<(), String> {
    let typed: T = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let back = serde_json::to_value(typed).map_err(|e| e.to_string())?;
    (&back == value)
        .then_some(())
        .ok_or_else(|| format!("{value} came back as {back}"))
}

methods! {
    "create" Mutation (CreateParams) -> Session,
        errors [InvalidRequest, LimitExceeded, Internal],
        examples [("params/create", "session/created")],
        "Starts a session: a program on a new PTY.";
    "list" Read (ListParams) -> Page,
        errors [InvalidRequest],
        examples [("params/list-first", "page/with-next"), ("params/list", "page/last")],
        "A page of sessions, ordered by id, without their records.";
    "inspect" Read (SessionParams) -> Session,
        errors [InvalidRequest, NoSuchSession],
        examples [("params/inspect", "session/inspect")],
        "One session, with its program status records.";
    "end" Mutation (SessionParams) -> Empty,
        errors [InvalidRequest, NoSuchSession, NotRunning, SignalFailed],
        examples [("params/end", "empty/basic")],
        "Sends SIGHUP to the session's processes.";
    "signal" Mutation (SignalParams) -> Empty,
        errors [InvalidRequest, NoSuchSession, NotRunning, SignalFailed],
        examples [("params/signal", "empty/basic")],
        "Sends a signal, by name, to the session's processes.";
    "remove" Mutation (SessionParams) -> Empty,
        errors [InvalidRequest, NoSuchSession, Running, Internal],
        examples [("params/remove", "empty/basic")],
        "Deletes a session that is not running, with its files.";
    "label" Mutation (LabelParams) -> Session,
        errors [InvalidRequest, NoSuchSession, LimitExceeded],
        examples [("params/label", "session/running")],
        "Sets the session's label.";
    "resize" Mutation (ResizeParams) -> Empty,
        errors [InvalidRequest, NoSuchSession, NotRunning],
        examples [("params/resize", "empty/basic"), ("params/resize-policy", "empty/basic")],
        "Sets the session's size, its size policy, or both.";
    "read" Read (ReadParams) -> Read,
        errors [InvalidRequest, NoSuchSession, ScopeUnavailable, EngineMismatch, Internal],
        examples [("params/read", "read/running"), ("params/read-scoped", "read/truncated")],
        "The session's screen, history or last command, as text, VT or HTML.";
    "tail" Read (TailParams) -> Tail,
        errors [InvalidRequest, NoSuchSession, Internal],
        examples [("params/tail", "tail/basic"), ("params/tail-from", "tail/truncated")],
        "The output stream as plain text, from a stream offset.";
    "send" Mutation (SendParams) -> Sent,
        errors [InvalidRequest, NoSuchSession, NotRunning, InputOverflow, Cancelled, Internal],
        examples [("params/send", "sent/plain"), ("params/send-text", "sent/wait")],
        "Writes text, named keys or bytes to the PTY; with `wait`, answers when it ends.";
    "wait" Read (WaitParams) -> Waited,
        errors [InvalidRequest, NoSuchSession, NotRunning, Cancelled],
        examples [("params/wait", "waited/status-done"), ("params/wait-output", "waited/timeout")],
        "Answers when the session exits, its output matches, a record is in a state, a prompt shows or output stops.";
    "cancel" Mutation (CancelParams) -> Cancelled,
        errors [InvalidRequest],
        examples [("params/cancel", "cancelled/yes")],
        "Ends a pending `wait`, or `send` with `wait`, of this connection.";
    "detach" Mutation (DetachParams) -> Empty,
        errors [InvalidRequest, NoSuchSession, NoSuchAttachment, LimitExceeded],
        examples [("params/detach", "empty/basic")],
        "Ends an attachment of any connection.";
    "subscribe" Stream (SubscribeParams) -> Subscribed,
        errors [InvalidRequest],
        examples [("params/subscribe", "subscribed/first"), ("params/subscribe-after", "subscribed/after")],
        "Starts `EVENT` frames on this connection: from now, or replayed after a `seq`.";
    "ack" Mutation (AckParams) -> Acked,
        errors [InvalidRequest, NoSuchSession],
        examples [("params/ack", "acked/basic")],
        "Marks `done` and `error` records seen.";
    "describe" Read (DescribeParams) -> Described,
        errors [InvalidRequest],
        examples [],
        "This catalog, or one method of it.";
}

/// The `describe` example asks for this method.
const DESCRIBE_EXAMPLE: &str = "params/describe-method";

/// What a frame is for.
fn frame_summary(frame_type: FrameType) -> &'static str {
    use FrameType::*;
    match frame_type {
        Hello => "Opens the connection: version, client and engine",
        Request => "A method call, answered by `REPLY` or `ERROR` with its `id`",
        Open => "Attaches the channel to a session",
        Input => "Bytes for the PTY",
        Viewport => "The attachment's size and profile",
        Close => "Detaches the channel",
        Welcome => "Accepts the connection: the server's engine, incarnation and limits",
        Reply => "A method's result",
        Event => "A change, once subscribed",
        Error => "A request's or an attachment's failure, or the connection's end",
        Attached => "The attachment's start: snapshot or resume",
        Snapshot => "Snapshot bytes",
        SnapshotReady => "The snapshot holds the screen; stream frames may follow",
        SnapshotEnd => "The last snapshot byte was sent",
        Output => "`offset u64` and PTY bytes from that stream offset",
        Resized => "A size marker in the stream",
        Exited => "The end of the stream",
        Gap => "Bytes were not delivered; a new bootstrap follows",
        Closed => "The attachment ended",
    }
}

/// The schema of a frame's JSON payload, if it has one.
fn frame_schema(
    frame_type: FrameType,
    de: &mut SchemaGenerator,
    ser: &mut SchemaGenerator,
) -> Option<schemars::Schema> {
    use FrameType::*;
    Some(match frame_type {
        Hello => de.subschema_for::<crate::Hello>(),
        Request => de.subschema_for::<crate::Request>(),
        Open => de.subschema_for::<crate::Open>(),
        Viewport => de.subschema_for::<crate::Viewport>(),
        Welcome => ser.subschema_for::<crate::Welcome>(),
        Reply => ser.subschema_for::<crate::Reply>(),
        Event => ser.subschema_for::<Numbered>(),
        Error => ser.subschema_for::<ErrorMessage>(),
        Attached => ser.subschema_for::<crate::Attached>(),
        Resized => ser.subschema_for::<crate::Resized>(),
        Exited => ser.subschema_for::<crate::Exited>(),
        Gap => ser.subschema_for::<crate::Gap>(),
        Closed => ser.subschema_for::<crate::Closed>(),
        Input | Close | Snapshot | SnapshotReady | SnapshotEnd | Output => return None,
    })
}

/// `SnapshotReady` as `SNAPSHOT_READY`.
fn frame_name(frame_type: FrameType) -> String {
    let mut name = String::new();
    for (i, c) in format!("{frame_type:?}").chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            name.push('_');
        }
        name.push(c.to_ascii_uppercase());
    }
    name
}

fn vector(vectors: &Map<String, Value>, name: &str) -> Value {
    vectors
        .get(name)
        .unwrap_or_else(|| panic!("no vector {name}"))
        .clone()
}

/// The catalog of this crate's protocol, for a server of release
/// `version`.
pub fn catalog(version: &str) -> Catalog {
    // Params are what the server reads; the rest is what it writes, so an
    // optional field reads as optional on both sides.
    let mut de = SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator();
    let mut ser = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator();
    let vectors: Map<String, Value> = serde_json::from_str(VECTORS).expect("vectors");

    let mut methods: Vec<MethodInfo> = specs()
        .into_iter()
        .map(|spec| MethodInfo {
            name: spec.name.into(),
            class: spec.class,
            summary: spec.summary.into(),
            params: (spec.params)(&mut de).to_value(),
            result: (spec.result)(&mut ser).to_value(),
            errors: spec.errors.to_vec(),
            examples: spec
                .examples
                .iter()
                .map(|(params, result)| Example {
                    params: vector(&vectors, params),
                    result: vector(&vectors, result),
                })
                .collect(),
        })
        .collect();

    let frames = FrameType::ALL
        .iter()
        .map(|&t| FrameInfo {
            name: frame_name(t),
            frame_type: t as u8,
            direction: if t.is_server() {
                Direction::Server
            } else {
                Direction::Client
            },
            channel: match t {
                FrameType::Error => "0 or n",
                t if t.is_control() => "0",
                _ => "n",
            }
            .into(),
            payload: match t {
                t if t.is_json() => Payload::Json,
                FrameType::Close | FrameType::SnapshotReady | FrameType::SnapshotEnd => {
                    Payload::Empty
                }
                _ => Payload::Raw,
            },
            summary: frame_summary(t).into(),
            schema: frame_schema(t, &mut de, &mut ser).map(|s| s.to_value()),
        })
        .collect();

    ser.subschema_for::<Event>();
    ser.subschema_for::<ErrorCode>();
    let mut sorted = BTreeMap::new();
    for (name, schema) in de.definitions().iter().chain(ser.definitions()) {
        if let Some(seen) = sorted.insert(name, schema) {
            assert_eq!(seen, schema, "{name} differs between params and results");
        }
    }
    let mut defs: Map<String, Value> = sorted
        .into_iter()
        .map(|(name, schema)| (name.clone(), schema.clone()))
        .collect();
    for schema in defs.values_mut() {
        tidy(schema);
    }
    for method in &mut methods {
        tidy(&mut method.params);
        tidy(&mut method.result);
    }

    let events = variants(&defs["Event"])
        .map(|branch| {
            let name = branch["properties"]["event"]["const"]
                .as_str()
                .expect("an event name")
                .to_string();
            let prefix = format!("event/{name}");
            EventInfo {
                summary: description(branch),
                schema: branch.clone(),
                examples: vectors
                    .iter()
                    .filter(|(key, _)| {
                        key.strip_prefix(&prefix)
                            .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
                    })
                    .map(|(_, v)| v.clone())
                    .collect(),
                name,
            }
        })
        .collect();
    let errors = variants(&defs["ErrorCode"])
        .map(|branch| ErrorInfo {
            code: serde_json::from_value(branch["const"].clone()).expect("an error code"),
            summary: description(branch),
        })
        .collect();

    let mut catalog = Catalog {
        protocol: VERSION,
        version: version.into(),
        methods,
        events,
        frames,
        errors,
        defs,
    };
    // `describe`'s example is the catalog's own answer.
    let params = vector(&vectors, DESCRIBE_EXAMPLE);
    let asked: DescribeParams = serde_json::from_value(params.clone()).expect("describe params");
    let answer = catalog
        .describe(asked.method.as_deref().expect("a method"))
        .expect("a method of the catalog");
    let describe = catalog
        .methods
        .iter_mut()
        .find(|m| m.name == "describe")
        .expect("describe");
    describe.examples.push(Example {
        params,
        result: serde_json::to_value(answer).expect("serializable"),
    });
    catalog
}

impl Catalog {
    /// The entry for `name` with the definitions it refers to, directly
    /// or through others.
    pub fn describe(&self, name: &str) -> Option<Description> {
        let method = self.method(name)?.clone();
        let mut wanted = Vec::new();
        refs(&method.params, &mut wanted);
        refs(&method.result, &mut wanted);
        let mut used = BTreeSet::new();
        while let Some(name) = wanted.pop() {
            if used.insert(name.clone())
                && let Some(schema) = self.defs.get(&name)
            {
                refs(schema, &mut wanted);
            }
        }
        Some(Description {
            protocol: self.protocol,
            version: self.version.clone(),
            method,
            defs: self
                .defs
                .iter()
                .filter(|(name, _)| used.contains(*name))
                .map(|(name, schema)| (name.clone(), schema.clone()))
                .collect(),
        })
    }

    pub fn method(&self, name: &str) -> Option<&MethodInfo> {
        self.methods.iter().find(|m| m.name == name)
    }
}

/// The names `#/$defs/NAME` references in `schema` lead to.
pub(crate) fn refs(schema: &Value, out: &mut impl Extend<String>) {
    match schema {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("$ref", Value::String(r)) => {
                        if let Some(name) = r.strip_prefix("#/$defs/") {
                            out.extend([name.to_string()]);
                        }
                    }
                    _ => refs(value, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
        _ => {}
    }
}

/// The `oneOf` branches of an enum's schema.
fn variants(schema: &Value) -> impl Iterator<Item = &Value> {
    schema["oneOf"].as_array().into_iter().flatten()
}

fn description(schema: &Value) -> String {
    schema["description"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Optional fields as the wire has them: absent, never `null` (schemars
/// lets an `Option` be `null` as well, which serde reads as absent, but
/// the protocol leaves an absent field out); and descriptions with doc
/// comments' line breaks undone.
fn tidy(schema: &mut Value) {
    let Value::Object(map) = schema else {
        if let Value::Array(items) = schema {
            items.iter_mut().for_each(tidy);
        }
        return;
    };
    if let Some(Value::String(text)) = map.get_mut("description") {
        *text = text
            .split("\n\n")
            .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    let required: BTreeSet<String> = map
        .get("required")
        .and_then(Value::as_array)
        .map(|r| {
            r.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        for (name, property) in properties.iter_mut() {
            if !required.contains(name) {
                without_null(property);
            }
        }
    }
    map.values_mut().for_each(tidy);
}

/// `schema` without its `null` alternative.
fn without_null(schema: &mut Value) {
    let null = json!({"type": "null"});
    let Value::Object(map) = schema else { return };
    if let Some(Value::Array(types)) = map.get_mut("type") {
        types.retain(|t| t != "null");
        if let [only] = types.as_slice() {
            let only = only.clone();
            map.insert("type".into(), only);
        }
    }
    if let Some(Value::Array(any)) = map.get("anyOf")
        && any.len() == 2
        && any.contains(&null)
    {
        let other = any.iter().find(|s| **s != null).cloned().expect("two");
        map.shift_remove("anyOf");
        if let Value::Object(other) = other {
            for (key, value) in other {
                map.entry(key).or_insert(value);
            }
        }
    }
}
