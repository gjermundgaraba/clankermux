use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use ghostty_vt::keys::{KeyEncoder, KeyPress, encode_paste};
use proto::{Closed, ClosedReason, ErrorCode, Event, Request, Sent, Size, Viewport};
use rustix::process::Signal;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::attach::Ending;
use crate::connection::{Writer, reply};
use crate::pty;
use crate::read;
use crate::registry::{Fail, Registry, internal};
use crate::session::{self, Shared, Source};
use crate::wait::{self, Facts, Waiter};

pub type Pending = Arc<Mutex<PendingMap>>;

#[derive(Default)]
pub struct PendingMap {
    by_id: HashMap<i64, (u64, oneshot::Sender<()>)>,
    serial: u64,
}

impl PendingMap {
    pub fn clear(&mut self) {
        self.by_id.clear();
    }
}

struct Claim {
    id: i64,
    serial: u64,
}

fn finish(pending: &Pending, writer: &Writer, claim: &Claim, outcome: Result<Value, Fail>) {
    let mut pending = pending.lock().unwrap();
    if pending
        .by_id
        .get(&claim.id)
        .is_some_and(|(serial, _)| *serial == claim.serial)
    {
        pending.by_id.remove(&claim.id);
        reply(writer, claim.id, outcome);
    }
}

fn spawn_wait(
    pending: &Pending,
    writer: &Writer,
    (claim, stop): (Claim, oneshot::Receiver<()>),
    session: Shared,
    waiter: Waiter,
    timeout: Option<Duration>,
    answer: impl FnOnce(proto::Waited) -> Value + Send + 'static,
) {
    let (pending, writer) = (pending.clone(), writer.clone());
    tokio::spawn(async move {
        let outcome = tokio::select! {
            biased;
            _ = stop => return,
            outcome = wait::run(&session, waiter, timeout) => outcome,
        };
        finish(&pending, &writer, &claim, outcome.map(answer));
    });
}

fn register(pending: &Pending, id: i64) -> (Claim, oneshot::Receiver<()>) {
    let (stop_tx, stop) = oneshot::channel();
    let mut pending = pending.lock().unwrap();
    pending.serial += 1;
    let serial = pending.serial;
    pending.by_id.insert(id, (serial, stop_tx));
    (Claim { id, serial }, stop)
}

fn params<T: DeserializeOwned>(value: Value) -> Result<T, Fail> {
    serde_json::from_value(value)
        .map_err(|e| (ErrorCode::InvalidRequest, format!("bad params: {e}")))
}

pub fn dispatch(
    registry: &Arc<Registry>,
    writer: &Writer,
    pending: &Pending,
    subscribed: &mut bool,
    request: Request,
) {
    let id = request.id;
    if registry.catalog().method(&request.method).is_none() {
        let fail = (
            ErrorCode::InvalidRequest,
            format!("unknown method {}", request.method),
        );
        return reply(writer, id, Err(fail));
    }
    match request.method.as_str() {
        "send" => send(registry, writer, pending, id, request.params),
        "wait" => {
            if let Err(fail) = wait(registry, writer, pending, id, request.params) {
                reply(writer, id, Err(fail));
            }
        }
        "cancel" => match params::<proto::CancelParams>(request.params) {
            Ok(p) => {
                let mut pending = pending.lock().unwrap();
                let cancelled = pending.by_id.remove(&p.id).is_some();
                if cancelled {
                    reply(
                        writer,
                        p.id,
                        Err((ErrorCode::Cancelled, format!("cancelled by request {id}"))),
                    );
                }
                reply(writer, id, Ok(json!(proto::Cancelled { cancelled })));
            }
            Err(fail) => reply(writer, id, Err(fail)),
        },
        "list" => match params::<proto::ListParams>(request.params) {
            Ok(p) => registry.reply_page(writer, id, p.after.as_deref()),
            Err(fail) => reply(writer, id, Err(fail)),
        },
        "subscribe" => match params::<proto::SubscribeParams>(request.params) {
            Ok(p) => {
                *subscribed |= registry.subscribe(writer, id, p, !*subscribed);
            }
            Err(fail) => reply(writer, id, Err(fail)),
        },
        _ => reply(writer, id, handle(registry, request)),
    }
}

enum Payload {
    Bytes(Vec<u8>),
    Keys(Vec<KeyPress>),
}

fn payload(p: &mut proto::SendParams) -> Result<Payload, Fail> {
    let invalid = |m: String| (ErrorCode::InvalidRequest, m);
    match (p.text.take(), p.keys.take(), p.data.take()) {
        (Some(text), None, None) => Ok(Payload::Bytes(text.into_bytes())),
        (None, Some(keys), None) => {
            if p.paste {
                return Err(invalid("paste takes text or data, not keys".into()));
            }
            keys.iter()
                .map(|name| {
                    KeyPress::parse(name).ok_or_else(|| invalid(format!("unknown key {name:?}")))
                })
                .collect::<Result<_, _>>()
                .map(Payload::Keys)
        }
        (None, None, Some(data)) => base64::engine::general_purpose::STANDARD
            .decode(&data)
            .map(Payload::Bytes)
            .map_err(|e| invalid(format!("data is not base64: {e}"))),
        _ => Err(invalid(
            "send takes exactly one of text, keys and data".into(),
        )),
    }
}

fn send(registry: &Arc<Registry>, writer: &Writer, pending: &Pending, id: i64, p: Value) {
    let started = (|| {
        let mut p: proto::SendParams = params(p)?;
        let payload = payload(&mut p)?;
        let facts = match p.wait {
            Some(w) => Some((Facts::new(w.until)?, w.timeout_ms)),
            None => None,
        };
        let session = registry.get(&p.session)?;
        let mut s = session.lock().unwrap();
        let live = s.running()?;
        let bytes = match payload {
            Payload::Bytes(bytes) if p.paste => {
                encode_paste(&bytes, live.term.mode(2004).unwrap_or(false)).map_err(internal)?
            }
            Payload::Bytes(bytes) => bytes,
            Payload::Keys(keys) => {
                let mut encoder = KeyEncoder::new().map_err(internal)?;
                let mut bytes = Vec::new();
                for key in &keys {
                    bytes.extend(encoder.encode(&live.term, key).map_err(internal)?);
                }
                bytes
            }
        };
        let at = s.cursor();
        let armed = facts.map(|(facts, timeout)| {
            (
                Waiter::for_send(facts, &s),
                timeout.map(Duration::from_millis),
            )
        });
        let written = s
            .running()?
            .input
            .push(bytes)
            .ok_or((ErrorCode::InputOverflow, "input queue full".into()))?;
        s.touch();
        let claim = armed.as_ref().map(|_| register(pending, id));
        let previous = s.send_chain.take();
        let (writer, pending) = (writer.clone(), pending.clone());
        let shared = session.clone();
        s.send_chain = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            if written.await.is_err() {
                let fail = internal("the PTY refused the bytes");
                match claim {
                    Some((claim, _)) => finish(&pending, &writer, &claim, Err(fail)),
                    None => reply(&writer, id, Err(fail)),
                }
                return;
            }
            let (Some((waiter, timeout)), Some(claim)) = (armed, claim) else {
                reply(&writer, id, Ok(json!(Sent { at, wait: None })));
                return;
            };
            let answer = move |waited| {
                json!(Sent {
                    at,
                    wait: Some(waited)
                })
            };
            spawn_wait(&pending, &writer, claim, shared, waiter, timeout, answer);
        }));
        Ok(())
    })();
    if let Err(fail) = started {
        reply(writer, id, Err(fail));
    }
}

fn wait(
    registry: &Arc<Registry>,
    writer: &Writer,
    pending: &Pending,
    id: i64,
    p: Value,
) -> Result<(), Fail> {
    let p: proto::WaitParams = params(p)?;
    let facts = Facts::new(p.until)?;
    let session = registry.get(&p.session)?;
    let waiter = Waiter::new(facts, wait::anchor(&session.lock().unwrap(), p.after)?);
    let timeout = p.timeout_ms.map(Duration::from_millis);
    let claim = register(pending, id);
    spawn_wait(pending, writer, claim, session, waiter, timeout, |waited| {
        json!(waited)
    });
    Ok(())
}

fn handle(registry: &Arc<Registry>, request: Request) -> Result<Value, Fail> {
    let p = request.params;
    match request.method.as_str() {
        "create" => Ok(json!(registry.create(params(p)?)?)),
        "inspect" => {
            let p: proto::SessionParams = params(p)?;
            let session = registry.get(&p.session)?;
            let out = session.lock().unwrap().to_proto(true);
            Ok(json!(out))
        }
        "end" => {
            let p: proto::SessionParams = params(p)?;
            send_signal(registry, &p.session, Signal::HUP)
        }
        "signal" => {
            let p: proto::SignalParams = params(p)?;
            let signal = pty::signal_by_name(&p.signal).ok_or((
                ErrorCode::InvalidRequest,
                format!("unknown signal {}", p.signal),
            ))?;
            send_signal(registry, &p.session, signal)
        }
        "remove" => {
            let p: proto::SessionParams = params(p)?;
            registry.remove(&p.session)?;
            Ok(json!({}))
        }
        "ack" => ack(registry, &params(p)?),
        "label" => label(registry, params(p)?),
        "resize" => resize(registry, &params(p)?),
        "detach" => detach(registry, &params(p)?),
        "describe" => {
            let p: proto::catalog::DescribeParams = params(p)?;
            let catalog = registry.catalog();
            match p.method {
                None => Ok(json!(catalog)),
                Some(name) => catalog
                    .describe(&name)
                    .map(|one| json!(one))
                    .ok_or((ErrorCode::InvalidRequest, format!("no method {name}"))),
            }
        }
        "read" => Ok(json!(read::read(registry, &params(p)?)?)),
        "tail" => Ok(json!(read::tail(registry, &params(p)?)?)),
        other => Err((ErrorCode::InvalidRequest, format!("unknown method {other}"))),
    }
}

fn send_signal(registry: &Registry, id: &str, signal: Signal) -> Result<Value, Fail> {
    let session = registry.get(id)?;
    let (pgid, slave) = {
        let s = session.lock().unwrap();
        let live = s.running()?;
        (live.pgid, live.slave_path.clone())
    };
    if pty::signal(pgid, &slave, signal) {
        Ok(json!({}))
    } else {
        Err((ErrorCode::SignalFailed, "no process to signal".into()))
    }
}

fn ack(registry: &Arc<Registry>, p: &proto::AckParams) -> Result<Value, Fail> {
    let session = registry.get(&p.session)?;
    let (record_ids, events) = {
        let mut s = session.lock().unwrap();
        let record_ids = s.ack(p.record_ids.as_deref());
        (record_ids, s.take_events())
    };
    registry.emit_all(events);
    if !record_ids.is_empty() {
        registry.records_changed(&session);
    }
    Ok(json!(proto::Acked { record_ids }))
}

fn label(registry: &Registry, p: proto::LabelParams) -> Result<Value, Fail> {
    if p.label.len() > proto::cap(registry.config.limits.label) {
        return Err((ErrorCode::LimitExceeded, "label too long".into()));
    }
    let session = registry.get(&p.session)?;
    let out = {
        let mut s = session.lock().unwrap();
        s.label = p.label;
        s.touch();
        s.to_proto(false)
    };
    registry.emit(Event::SessionChanged {
        session: out.clone(),
    });
    Ok(json!(out))
}

fn resize(registry: &Registry, p: &proto::ResizeParams) -> Result<Value, Fail> {
    if p.cols.is_none() && p.rows.is_none() && p.policy.is_none() {
        return Err((
            ErrorCode::InvalidRequest,
            "cols, rows or policy is needed".into(),
        ));
    }
    if p.cols == Some(0) || p.rows == Some(0) {
        return Err((
            ErrorCode::InvalidRequest,
            "cols and rows must be at least 1".into(),
        ));
    }
    let session = registry.get(&p.session)?;
    let mut s = session.lock().unwrap();
    s.running()?;
    let mut changed = p.policy.is_some_and(|policy| s.set_policy(policy));
    if p.cols.is_some() || p.rows.is_some() {
        let current = s.size();
        let report = Viewport {
            size: Size {
                cols: p.cols.unwrap_or(current.cols),
                rows: p.rows.unwrap_or(current.rows),
                cell_width: None,
                cell_height: None,
            },
            profile: None,
        };
        changed |= s.apply_report(report, Source::Resize);
    }
    let changed = changed.then(|| s.to_proto(false));
    let events = s.take_events();
    drop(s);
    registry.emit_session(events, changed);
    Ok(json!({}))
}

fn detach(registry: &Registry, p: &proto::DetachParams) -> Result<Value, Fail> {
    if p.reason
        .as_ref()
        .is_some_and(|r| r.len() > proto::cap(registry.config.limits.text))
    {
        return Err((ErrorCode::LimitExceeded, "reason too long".into()));
    }
    let session = registry.get(&p.session)?;
    let s = session.lock().unwrap();
    let attachment = s
        .attached
        .iter()
        .find(|a| a.id == p.channel)
        .ok_or_else(|| {
            (
                ErrorCode::NoSuchAttachment,
                format!("no attachment {} on session {}", p.channel, p.session),
            )
        })?;
    if let Some(close) = attachment.close.upgrade() {
        let _ = close.send(Ending::Closed(Closed {
            reason: ClosedReason::Detached,
            message: p.reason.as_deref().map(session::printable),
        }));
    }
    Ok(json!({}))
}
