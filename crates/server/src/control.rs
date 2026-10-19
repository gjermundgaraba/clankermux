//! Channel 0 requests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use ghostty_vt::keys::{KeyEncoder, KeyPress, encode_paste};
use proto::{ErrorCode, Event, Request, Sent, Size, Viewport};
use rustix::process::Signal;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::connection::{Writer, reply};
use crate::registry::{Fail, Registry, internal};
use crate::session::Shared;
use crate::wait::{self, Facts, Waiter};
use crate::{pty, session};

/// A connection's requests that answer later and can be cancelled (a
/// `wait`, or a `send` carrying one), by id, with what stops each. The
/// lock is held while the answer is queued, so a `cancel` and the answer
/// cannot cross: whoever removes the entry answers it.
pub type Pending = Arc<Mutex<PendingMap>>;

#[derive(Default)]
pub struct PendingMap {
    /// Per id: the serial of the request that holds it, and its stop.
    by_id: HashMap<i64, (u64, oneshot::Sender<()>)>,
    serial: u64,
}

impl PendingMap {
    /// Drops every entry, which stops their waits.
    pub fn clear(&mut self) {
        self.by_id.clear();
    }
}

/// One pending request's claim on its id.
struct Claim {
    id: i64,
    serial: u64,
}

/// Answers the pending request `claim` with `outcome`, unless a `cancel`
/// (or the connection's end) took it first; a later request that reused
/// the id after a cancel is not this one.
fn finish(pending: &Pending, writer: &Writer, claim: Claim, outcome: Result<Value, Fail>) {
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

/// Registers `id` as pending and runs `waiter` on a task until it fires,
/// fails, or the request is cancelled; `answer` makes the reply.
fn spawn_wait(
    pending: &Pending,
    writer: &Writer,
    id: i64,
    session: Shared,
    waiter: Waiter,
    timeout: Option<Duration>,
    answer: impl FnOnce(proto::Waited) -> Value + Send + 'static,
) {
    let (claim, stop) = register(pending, id);
    let (pending, writer) = (pending.clone(), writer.clone());
    tokio::spawn(async move {
        let outcome = tokio::select! {
            biased;
            _ = stop => return,
            outcome = wait::run(&session, waiter, timeout) => outcome,
        };
        finish(&pending, &writer, claim, outcome.map(answer));
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

/// JSON overhead allowed around `read`'s text within `max_frame`.
const READ_REPLY_MARGIN: usize = 4096;

/// The bytes `text` takes as a JSON string, without the quotes.
fn json_len(text: &str) -> usize {
    serde_json::to_string(text).map_or(usize::MAX, |s| s.len() - 2)
}

/// Drops the oldest lines of `text` until its JSON string fits `budget`;
/// when not even the last line fits, its newest characters are kept.
fn fit_json(text: &mut String, budget: usize) {
    if json_len(text) <= budget {
        return;
    }
    let mut kept = 0;
    let mut start = text.len();
    for line in text.split_inclusive('\n').rev() {
        let len = json_len(line);
        if kept + len > budget {
            if kept == 0 {
                for (at, ch) in line.char_indices().rev() {
                    let len = json_len(ch.encode_utf8(&mut [0; 4]));
                    if kept + len > budget {
                        break;
                    }
                    kept += len;
                    start = text.len() - line.len() + at;
                }
            }
            break;
        }
        kept += len;
        start -= line.len();
    }
    text.drain(..start);
}

fn params<T: DeserializeOwned>(value: Value) -> Result<T, Fail> {
    serde_json::from_value(value)
        .map_err(|e| (ErrorCode::InvalidRequest, format!("bad params: {e}")))
}

/// Answers `request` on `writer`: `send` from a task once the PTY took
/// the bytes, `list` and `subscribe` from the registry under its lock,
/// the rest at once.
pub fn dispatch(
    registry: &Arc<Registry>,
    writer: &Writer,
    pending: &Pending,
    subscribed: &mut bool,
    request: Request,
) {
    let id = request.id;
    match request.method.as_str() {
        "send" => send(registry, writer, pending, id, request.params),
        "wait" => {
            if let Err(fail) = wait(registry, writer, pending, id, request.params) {
                reply(writer, id, Err(fail));
            }
        }
        "cancel" => match params::<proto::CancelParams>(request.params) {
            Ok(p) => {
                // The cancelled request's last frame comes first.
                let mut pending = pending.lock().unwrap();
                let stop = pending.by_id.remove(&p.id);
                if let Some(stop) = &stop {
                    let _ = stop;
                    reply(
                        writer,
                        p.id,
                        Err((ErrorCode::Cancelled, format!("cancelled by request {id}"))),
                    );
                }
                reply(
                    writer,
                    id,
                    Ok(json!(proto::Cancelled {
                        cancelled: stop.is_some()
                    })),
                );
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

/// What a `send` writes, before the session's terminal encodes it.
enum Payload {
    Bytes(Vec<u8>),
    Keys(Vec<KeyPress>),
}

/// The payload of a `send`: exactly one of text, keys and data, every key
/// known.
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

/// `send`: answered once the PTY took the bytes, or with a wait once that
/// fires; meanwhile the connection reads on. The keys are encoded and a
/// paste bracketed by the session's terminal as the bytes are queued, and
/// the wait is armed at the cursor then. The write's task is chained on
/// the session's previous one under the session lock, so the bytes and
/// the replies without a wait come in queue order; a failure is answered
/// at once.
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
        // Where the bytes are accepted: the reply's `at`, and where a
        // wait is armed.
        let at = wait::cursor_of(&s);
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
        // Pending from here, so a cancel that follows finds it.
        let claim = armed.as_ref().map(|_| register(pending, id));
        let previous = s.send_chain.take();
        let (registry, writer, pending) = (registry.clone(), writer.clone(), pending.clone());
        let shared = session.clone();
        s.send_chain = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            if written.await.is_err() {
                let fail = internal("the PTY refused the bytes");
                match claim {
                    Some((claim, _)) => finish(&pending, &writer, claim, Err(fail)),
                    None => reply(&writer, id, Err(fail)),
                }
                return;
            }
            let (Some((waiter, timeout)), Some((claim, stop))) = (armed, claim) else {
                reply(&writer, id, Ok(json!(Sent { at, wait: None })));
                return;
            };
            // The wait goes on off the chain, so later sends answer.
            drop(registry);
            tokio::spawn(async move {
                let outcome = tokio::select! {
                    biased;
                    _ = stop => return,
                    outcome = wait::run(&shared, waiter, timeout) => outcome,
                };
                let outcome = outcome.map(|waited| {
                    json!(Sent {
                        at,
                        wait: Some(waited)
                    })
                });
                finish(&pending, &writer, claim, outcome);
            });
        }));
        Ok(())
    })();
    if let Err(fail) = started {
        reply(writer, id, Err(fail));
    }
}

/// `wait`: anchored at `after`, or the cursor now, and answered from a
/// task once a fact holds.
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
    spawn_wait(pending, writer, id, session, waiter, timeout, |waited| {
        json!(waited)
    });
    Ok(())
}

/// Handles one request but `send`, `list` and `subscribe`.
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
        "end" | "signal" => {
            let (id, signal) = if request.method == "end" {
                (params::<proto::SessionParams>(p)?.session, Signal::HUP)
            } else {
                let p: proto::SignalParams = params(p)?;
                let signal = pty::signal_by_name(&p.signal).ok_or((
                    ErrorCode::InvalidRequest,
                    format!("unknown signal {}", p.signal),
                ))?;
                (p.session, signal)
            };
            let session = registry.get(&id)?;
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
        "remove" => {
            let p: proto::SessionParams = params(p)?;
            registry.remove(&p.session)?;
            Ok(json!({}))
        }
        "ack" => {
            let p: proto::AckParams = params(p)?;
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
        "label" => {
            let p: proto::LabelParams = params(p)?;
            if p.label.len() > registry.config.limits.label as usize {
                return Err((ErrorCode::LimitExceeded, "label too long".into()));
            }
            let session = registry.get(&p.session)?;
            let out = {
                let mut s = session.lock().unwrap();
                s.label = p.label;
                s.touch();
                s.to_proto(false)
            };
            registry.emit(&Event::SessionChanged {
                session: out.clone(),
            });
            Ok(json!(out))
        }
        "resize" => {
            let p: proto::ResizeParams = params(p)?;
            if p.cols.is_none() && p.rows.is_none() {
                return Err((ErrorCode::InvalidRequest, "cols or rows is needed".into()));
            }
            if p.cols == Some(0) || p.rows == Some(0) {
                return Err((
                    ErrorCode::InvalidRequest,
                    "cols and rows must be at least 1".into(),
                ));
            }
            let session = registry.get(&p.session)?;
            let current = {
                let s = session.lock().unwrap();
                s.running()?;
                s.size()
            };
            let size = Size {
                cols: p.cols.unwrap_or(current.cols),
                rows: p.rows.unwrap_or(current.rows),
                cell_width: None,
                cell_height: None,
            };
            let report = Viewport {
                size,
                profile: None,
            };
            session::report_now(registry, &session, report);
            Ok(json!({}))
        }
        "read" => {
            let p: proto::ReadParams = params(p)?;
            let session = registry.get(&p.session)?;
            let s = session.lock().unwrap();
            let mut text = s
                .read_text(p.scrollback.unwrap_or(0) as usize)
                .map_err(internal)?;
            if !text.is_empty() {
                text.push('\n');
            }
            fit_json(
                &mut text,
                (registry.config.limits.max_frame as usize).saturating_sub(READ_REPLY_MARGIN),
            );
            let info = s.to_proto(false);
            Ok(json!(proto::Read {
                text,
                cols: info.cols,
                rows: info.rows,
                cursor: s.live().map(|live| {
                    let (x, y) = live.term.cursor();
                    proto::Cursor { x, y }
                }),
                offset: info.offset,
                status: s.status(),
            }))
        }
        other => Err((ErrorCode::InvalidRequest, format!("unknown method {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::fit_json;

    #[test]
    fn fitting_keeps_the_newest_lines_or_the_tail_of_one() {
        // "bb\n" is four JSON bytes: the newline escapes to two.
        let mut text = "aaaa\nbb\ncc\n".to_string();
        fit_json(&mut text, 8);
        assert_eq!(text, "bb\ncc\n");
        let mut text = "xxxxxxxxxx\n".to_string();
        fit_json(&mut text, 4);
        assert_eq!(text, "xx\n");
        let mut quoted = "a\"b\n".to_string();
        fit_json(&mut quoted, 3);
        assert_eq!(quoted, "b\n");
    }
}
