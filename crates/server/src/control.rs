//! Channel 0 requests.

use std::sync::Arc;

use base64::Engine as _;
use ghostty_vt::formatter;
use proto::{ErrorCode, Event, Request, Status};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::registry::{Fail, Registry};

pub struct Conn {
    pub tx: mpsc::UnboundedSender<Vec<u8>>,
}

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

pub async fn handle(
    registry: &Arc<Registry>,
    conn: &Conn,
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
            let number = crate::pty::signal_number(&signal).ok_or((
                ErrorCode::InvalidRequest,
                format!("unknown signal {signal}"),
            ))?;
            let session = registry.get(&id)?;
            let (pgid, slave) = {
                let s = session.lock().unwrap();
                running(s.status)?;
                (s.pgid, s.slave_path.clone())
            };
            if crate::pty::signal(pgid, &slave, number) {
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
            if p.label.len() > registry.limits().label as usize {
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
            let session = registry.get(&p.session)?;
            let changed = {
                let mut s = session.lock().unwrap();
                running(s.status)?;
                s.resize(p.cols, p.rows, 0, 0).map(|_| s.to_proto(false))
            };
            if let Some(out) = changed {
                registry.emit(&Event::SessionChanged { session: out });
            }
            Ok(json!({}))
        }
        "read" => {
            let p: proto::ReadParams = params(p)?;
            let session = registry.get(&p.session)?;
            let s = session.lock().unwrap();
            let formatted = formatter::format(
                &s.term,
                formatter::Options {
                    unwrap: true,
                    trim: true,
                    ..Default::default()
                },
            )
            .map_err(crate::registry::internal)?;
            let text = String::from_utf8_lossy(&formatted);
            let lines: Vec<&str> = text.lines().collect();
            let keep = s.term.rows() as usize + p.scrollback.unwrap_or(0) as usize;
            let mut text = lines[lines.len().saturating_sub(keep)..].join("\n");
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
            let _ = wait.await;
            Ok(json!({}))
        }
        "subscribe" => {
            registry.subscribe(conn.tx.clone());
            Ok(json!(registry.page(None)))
        }
        other => Err((ErrorCode::InvalidRequest, format!("unknown method {other}"))),
    }
}
