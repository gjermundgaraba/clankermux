//! Channel 0 requests.

use std::sync::Arc;

use base64::Engine as _;
use ghostty_vt::formatter;
use proto::{ErrorCode, Event, Request, Size, Status, Viewport};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::connection::Writer;
use crate::registry::{Fail, Registry, internal};
use crate::session::{self, Report};

/// JSON overhead allowed around `read`'s text within `max_frame`.
const READ_REPLY_MARGIN: usize = 4096;

fn params<T: DeserializeOwned>(value: Value) -> Result<T, Fail> {
    serde_json::from_value(value)
        .map_err(|e| (ErrorCode::InvalidRequest, format!("bad params: {e}")))
}

fn running(status: Status) -> Result<(), Fail> {
    if status == Status::Running {
        Ok(())
    } else {
        Err((ErrorCode::NotRunning, "session is not running".into()))
    }
}

/// Handles one request on a connection whose `subscribed` flag records
/// whether it already receives events.
pub async fn handle(
    registry: &Arc<Registry>,
    writer: &Writer,
    subscribed: &mut bool,
    request: Request,
) -> Result<Value, Fail> {
    let p = request.params;
    match request.method.as_str() {
        "create" => Ok(json!(registry.create(params(p)?)?)),
        "list" => {
            let p: proto::ListParams = params(p)?;
            Ok(json!(registry.page(p.after.as_deref())))
        }
        "inspect" => {
            let p: proto::SessionParams = params(p)?;
            let session = registry.get(&p.session)?;
            let out = session.lock().unwrap().to_proto(true);
            Ok(json!(out))
        }
        "end" | "signal" => {
            let (id, signal) = if request.method == "end" {
                (
                    params::<proto::SessionParams>(p)?.session,
                    "HUP".to_string(),
                )
            } else {
                let p: proto::SignalParams = params(p)?;
                (p.session, p.signal)
            };
            let signal = crate::pty::signal_by_name(&signal).ok_or((
                ErrorCode::InvalidRequest,
                format!("unknown signal {signal}"),
            ))?;
            let session = registry.get(&id)?;
            let (pgid, slave) = {
                let s = session.lock().unwrap();
                running(s.status)?;
                (s.pgid, s.slave_path.clone())
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
            if p.cols == 0 || p.rows == 0 {
                return Err((
                    ErrorCode::InvalidRequest,
                    "cols and rows must be at least 1".into(),
                ));
            }
            let session = registry.get(&p.session)?;
            running(session.lock().unwrap().status)?;
            let report = Report {
                source: None,
                viewport: Viewport {
                    size: Size {
                        cols: p.cols,
                        rows: p.rows,
                        cell_width: None,
                        cell_height: None,
                    },
                    profile: None,
                },
            };
            session::report(registry, &session, report, true);
            Ok(json!({}))
        }
        "read" => {
            let p: proto::ReadParams = params(p)?;
            let session = registry.get(&p.session)?;
            let s = session.lock().unwrap();
            let rows = usize::from(s.term.rows());
            let wanted = rows + p.scrollback.unwrap_or(0) as usize;
            let start_row = (s.term.scrollback_rows() + rows).saturating_sub(wanted);
            let formatted = formatter::format(
                &s.term,
                formatter::Options {
                    unwrap: true,
                    trim: true,
                    start_row: Some(start_row as u32),
                    ..Default::default()
                },
            )
            .map_err(internal)?;
            let mut text = String::from_utf8_lossy(&formatted).into_owned();
            let max = (registry.config.limits.max_frame as usize).saturating_sub(READ_REPLY_MARGIN);
            if text.len() > max {
                let excess = text.len() - max;
                let cut = text[excess..]
                    .find('\n')
                    .map_or(text.len(), |i| excess + i + 1);
                text.drain(..cut);
            }
            if !text.is_empty() {
                text.push('\n');
            }
            let (x, y) = s.term.cursor();
            Ok(json!(proto::Read {
                text,
                cols: s.term.cols(),
                rows: s.term.rows(),
                cursor: (s.status != Status::Restored).then_some(proto::Cursor { x, y }),
                offset: s.stream.offset(),
                status: s.status,
            }))
        }
        "write" => {
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
            let wait = {
                let s = session.lock().unwrap();
                running(s.status)?;
                s.input
                    .push(data)
                    .ok_or((ErrorCode::InputOverflow, "input queue full".into()))?
            };
            wait.await
                .map_err(|_| internal("the PTY refused the bytes"))?;
            Ok(json!({}))
        }
        "subscribe" => {
            if *subscribed {
                return Ok(json!(registry.page(None)));
            }
            *subscribed = true;
            Ok(json!(registry.subscribe(writer.clone())))
        }
        other => Err((ErrorCode::InvalidRequest, format!("unknown method {other}"))),
    }
}
