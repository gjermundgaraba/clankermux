//! Channel 0 requests.

use std::sync::Arc;

use base64::Engine as _;
use proto::{ErrorCode, Event, Request, Size, Viewport};
use rustix::process::Signal;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::connection::{Writer, reply};
use crate::registry::{Fail, Registry, internal};
use crate::session;

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

/// Answers `request` on `writer`: `write` from a task once the PTY took
/// the bytes, `list` and `subscribe` from the registry under its lock,
/// the rest at once.
pub fn dispatch(
    registry: &Arc<Registry>,
    writer: &Writer,
    subscribed: &mut bool,
    request: Request,
) {
    let id = request.id;
    match request.method.as_str() {
        "write" => write(registry, writer, id, request.params),
        "list" => match params::<proto::ListParams>(request.params) {
            Ok(p) => registry.reply_page(writer, id, p.after.as_deref(), false),
            Err(fail) => reply(writer, id, Err(fail)),
        },
        "subscribe" => {
            let first = !std::mem::replace(subscribed, true);
            registry.reply_page(writer, id, None, first);
        }
        _ => reply(writer, id, handle(registry, request)),
    }
}

/// `write`: answered once the PTY took the bytes, meanwhile the
/// connection reads on. The reply task is chained on the session's
/// previous one under the session lock, as the bytes are queued, so the
/// replies come in queue order; a failure is answered at once.
fn write(registry: &Arc<Registry>, writer: &Writer, id: i64, p: Value) {
    let started = (|| {
        let p: proto::WriteParams = params(p)?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(&p.data)
            .map_err(|e| {
                (
                    ErrorCode::InvalidRequest,
                    format!("data is not base64: {e}"),
                )
            })?;
        let session = registry.get(&p.session)?;
        let mut s = session.lock().unwrap();
        let wait = s
            .running()?
            .input
            .push(data)
            .ok_or((ErrorCode::InputOverflow, "input queue full".into()))?;
        let previous = s.write_chain.take();
        let writer = writer.clone();
        s.write_chain = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let outcome = wait
                .await
                .map(|()| json!({}))
                .map_err(|_| internal("the PTY refused the bytes"));
            reply(&writer, id, outcome);
        }));
        Ok(())
    })();
    if let Err(fail) = started {
        reply(writer, id, Err(fail));
    }
}

/// Handles one request but `write`, `list` and `subscribe`.
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
                let signal = crate::pty::signal_by_name(&p.signal).ok_or((
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
            if crate::pty::signal(pgid, &slave, signal) {
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
        "set_label" => {
            let p: proto::SetLabelParams = params(p)?;
            if p.label.len() > registry.config.limits.label as usize {
                return Err((ErrorCode::LimitExceeded, "label too long".into()));
            }
            let session = registry.get(&p.session)?;
            let out = {
                let mut s = session.lock().unwrap();
                s.label = p.label;
                s.to_proto(false)
            };
            registry.emit(&Event::SessionChanged {
                session: out.clone(),
            });
            Ok(json!(out))
        }
        "set_size" => {
            let p: proto::SetSizeParams = params(p)?;
            let size = Size {
                cols: p.cols,
                rows: p.rows,
                cell_width: None,
                cell_height: None,
            };
            session::check_size(&size)?;
            let session = registry.get(&p.session)?;
            session.lock().unwrap().running()?;
            let report = Viewport {
                size,
                profile: None,
            };
            session::report(registry, &session, report, true);
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
