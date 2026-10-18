//! One attachment: its bootstrap, the stream from its cursor, and its end.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ghostty_vt::snapshot;
use proto::{
    AttachMode, Attached, Closed, ClosedReason, ErrorCode, ErrorMessage, Event, Exited, FrameType,
    Gap, Open, Resized, Resume, Status, Viewport, encode_frame, encode_output,
};
use tokio::sync::mpsc;

use crate::connection::Writer;
use crate::registry::Registry;
use crate::session::{Attachment, Report, Session, Shared};
use crate::stream::MarkerKind;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Bytes pulled from the stream per lock hold.
const PULL_BUDGET: usize = 64 * 1024;

/// How long a snapshot is retried while the continuation limit is exceeded.
const SNAPSHOT_RETRY: Duration = Duration::from_secs(5);

/// The connection's side of an attachment.
pub struct Handle {
    pub id: u64,
    pub session: Shared,
    pub read_only: bool,
    close: mpsc::UnboundedSender<ClosedReason>,
    task: tokio::task::JoinHandle<()>,
}

impl Handle {
    /// Whether the attachment task is still serving the channel.
    pub fn is_open(&self) -> bool {
        !self.task.is_finished()
    }

    /// `CLOSE`: the task sends `CLOSED closed` after its pending frames.
    pub fn close(self) {
        let _ = self.close.send(ClosedReason::Closed);
    }
}

/// Starts serving `open` on `channel`. The checks that precede this
/// (channel, session, engine) are the connection's.
pub fn open(
    registry: &Arc<Registry>,
    writer: &Writer,
    channel: u32,
    open: Open,
    session: Shared,
) -> Handle {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (close_tx, close_rx) = mpsc::unbounded_channel();
    let finished = Arc::new(AtomicBool::new(false));
    let snapshot = {
        let mut s = session.lock().unwrap();
        // A writable OPEN's viewport and profile are one immediate report.
        if !open.read_only && (open.viewport.is_some() || open.profile.is_some()) {
            let size = open.viewport.unwrap_or_else(|| s.size());
            s.apply_report(Report {
                source: Some(id),
                viewport: Viewport {
                    size,
                    profile: open.profile,
                },
            });
        }
        s.attached.push(Attachment {
            id,
            close: close_tx.downgrade(),
            finished: finished.clone(),
        });
        s.to_proto(false)
    };
    registry.emit(&Event::SessionChanged { session: snapshot });
    let task = Task {
        id,
        channel,
        writer: writer.clone(),
        registry: registry.clone(),
        session: session.clone(),
        close: close_rx,
        finished,
        max_frame: registry.config.limits.max_frame as usize,
        cursor: Cursor {
            offset: 0,
            marker: 0,
        },
        snapshot: None,
        done: false,
    };
    Handle {
        id,
        session,
        read_only: open.read_only,
        close: close_tx,
        task: tokio::spawn(task.run(open.resume)),
    }
}

#[derive(Clone, Copy)]
struct Cursor {
    offset: u64,
    marker: u64,
}

/// Snapshot bytes not yet sent.
struct PendingSnapshot {
    bytes: Vec<u8>,
    sent: usize,
}

/// How an attachment ends: with a `CLOSED`, or silently after an `ERROR`
/// or because the connection dropped it.
enum End {
    Closed(ClosedReason),
    Silent,
}

impl From<Option<ClosedReason>> for End {
    fn from(reason: Option<ClosedReason>) -> Self {
        reason.map_or(End::Silent, End::Closed)
    }
}

enum Pull {
    Sent,
    Idle,
    Gap,
}

struct Task {
    id: u64,
    channel: u32,
    writer: Writer,
    registry: Arc<Registry>,
    session: Shared,
    close: mpsc::UnboundedReceiver<ClosedReason>,
    finished: Arc<AtomicBool>,
    max_frame: usize,
    cursor: Cursor,
    snapshot: Option<PendingSnapshot>,
    /// The final marker has been sent.
    done: bool,
}

impl Task {
    async fn run(mut self, resume: Option<Resume>) {
        let end = self.serve(resume).await;
        if let End::Closed(reason) = end {
            self.send_json(FrameType::Closed, &Closed { reason });
        }
        let snapshot = {
            let mut s = self.session.lock().unwrap();
            s.attached.retain(|a| a.id != self.id);
            s.to_proto(false)
        };
        if !matches!(
            end,
            End::Closed(ClosedReason::Removed | ClosedReason::Stopping)
        ) {
            self.registry
                .emit(&Event::SessionChanged { session: snapshot });
        }
    }

    fn send(&self, frame_type: FrameType, payload: &[u8]) {
        self.writer
            .send(encode_frame(frame_type, self.channel, payload));
    }

    fn send_json<T: serde::Serialize>(&self, frame_type: FrameType, body: &T) {
        self.writer.send_json(frame_type, self.channel, body);
    }

    fn error(&self, code: ErrorCode, message: impl Into<String>) {
        self.send_json(
            FrameType::Error,
            &ErrorMessage {
                id: None,
                code,
                message: message.into(),
            },
        );
    }

    async fn serve(&mut self, resume: Option<Resume>) -> End {
        match self.resume_cursor(resume) {
            Err(end) => return end,
            Ok(Some(cursor)) => {
                let (cols, rows, session) = {
                    let s = self.session.lock().unwrap();
                    (s.term.cols(), s.term.rows(), s.to_proto(false))
                };
                self.cursor = cursor;
                self.send_json(
                    FrameType::Attached,
                    &Attached {
                        mode: AttachMode::Resume,
                        offset: cursor.offset,
                        marker: cursor.marker,
                        cols,
                        rows,
                        session,
                    },
                );
            }
            Ok(None) => {
                if let Err(end) = self.bootstrap(None).await {
                    return end;
                }
            }
        }
        loop {
            let wake = self.session.lock().unwrap().wake.clone();
            let notified = wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.writer.over_high_water() {
                tokio::select! {
                    reason = self.close.recv() => return reason.into(),
                    _ = self.writer.drained() => {}
                }
                continue;
            }
            match self.pull() {
                Pull::Sent => continue,
                Pull::Gap => {
                    if self.snapshot.is_some() {
                        self.error(ErrorCode::TooSlow, "fell behind during a bootstrap");
                        return End::Silent;
                    }
                    if let Err(end) = self.bootstrap(Some(self.cursor.offset)).await {
                        return end;
                    }
                }
                Pull::Idle => {
                    if self.send_snapshot_chunk() {
                        continue;
                    }
                    if self.done {
                        self.finished.store(true, Ordering::Release);
                    }
                    tokio::select! {
                        reason = self.close.recv() => return reason.into(),
                        _ = notified => {}
                    }
                }
            }
        }
    }

    /// The cursor to resume from, when `resume` qualifies.
    fn resume_cursor(&self, resume: Option<Resume>) -> Result<Option<Cursor>, End> {
        let Some(resume) = resume.filter(|r| r.incarnation == self.registry.incarnation) else {
            return Ok(None);
        };
        let s = self.session.lock().unwrap();
        if resume.marker > s.stream.latest_seq() {
            self.error(ErrorCode::InvalidRequest, "resume marker above the latest");
            return Err(End::Silent);
        }
        let retained = s.stream.retained_start() <= resume.offset
            && resume.offset <= s.stream.offset()
            && s.stream.markers_retained_after(resume.marker);
        Ok(retained.then_some(Cursor {
            offset: resume.offset,
            marker: resume.marker,
        }))
    }

    /// Takes a snapshot, retrying while the continuation limit is exceeded,
    /// then sends `GAP` (after a gap), `ATTACHED`, the snapshot through
    /// `READY` and `SNAPSHOT_READY`. The rest of the snapshot is sent as
    /// the stream allows.
    async fn bootstrap(&mut self, gap_from: Option<u64>) -> Result<(), End> {
        let deadline = Instant::now() + SNAPSHOT_RETRY;
        loop {
            let wake = self.session.lock().unwrap().wake.clone();
            let notified = wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let taken = {
                let s = self.session.lock().unwrap();
                tokio::task::block_in_place(|| snapshot::encode(&s.term)).map(|bytes| {
                    (
                        bytes,
                        Cursor {
                            offset: s.stream.offset(),
                            marker: s.stream.latest_seq(),
                        },
                        s.to_proto(false),
                    )
                })
            };
            match taken {
                Ok((bytes, cursor, session)) => {
                    if let Some(from) = gap_from {
                        self.send_json(
                            FrameType::Gap,
                            &Gap {
                                from,
                                to: cursor.offset,
                            },
                        );
                    }
                    self.cursor = cursor;
                    self.done = false;
                    self.send_json(
                        FrameType::Attached,
                        &Attached {
                            mode: AttachMode::Snapshot,
                            offset: cursor.offset,
                            marker: cursor.marker,
                            cols: session.cols,
                            rows: session.rows,
                            session,
                        },
                    );
                    let ready = snapshot::ready_end(&bytes).expect("a snapshot has READY");
                    self.snapshot = Some(PendingSnapshot { bytes, sent: 0 });
                    while self.snapshot.as_ref().is_some_and(|p| p.sent < ready) {
                        self.send_snapshot_bytes(ready);
                    }
                    self.send(FrameType::SnapshotReady, &[]);
                    return Ok(());
                }
                Err(ghostty_vt::Error::LimitExceeded) if Instant::now() < deadline => {
                    tokio::select! {
                        reason = self.close.recv() => return Err(reason.into()),
                        _ = notified => {}
                        _ = tokio::time::sleep_until((deadline).into()) => {}
                    }
                }
                Err(err) => {
                    let code = match err {
                        ghostty_vt::Error::LimitExceeded => ErrorCode::LimitExceeded,
                        _ => ErrorCode::Internal,
                    };
                    self.error(code, format!("snapshot failed: {err}"));
                    return Err(End::Silent);
                }
            }
        }
    }

    /// Sends one `SNAPSHOT` frame up to `limit`.
    fn send_snapshot_bytes(&mut self, limit: usize) {
        let Some(pending) = self.snapshot.as_mut() else {
            return;
        };
        let end = limit.min(pending.sent + self.max_frame);
        let chunk = pending.bytes[pending.sent..end].to_vec();
        pending.sent = end;
        self.send(FrameType::Snapshot, &chunk);
    }

    /// Sends the next remaining snapshot chunk, and `SNAPSHOT_END` after
    /// the last. False when no snapshot is pending.
    fn send_snapshot_chunk(&mut self) -> bool {
        let Some(pending) = self.snapshot.as_ref() else {
            return false;
        };
        let total = pending.bytes.len();
        if pending.sent < total {
            self.send_snapshot_bytes(total);
        }
        if self.snapshot.as_ref().is_some_and(|p| p.sent >= total) {
            self.snapshot = None;
            self.send(FrameType::SnapshotEnd, &[]);
        }
        true
    }

    /// Sends the stream from the cursor: each marker before any byte at or
    /// past its offset, up to `PULL_BUDGET` bytes.
    fn pull(&mut self) -> Pull {
        let mut frames = Vec::new();
        {
            let s = self.session.lock().unwrap();
            let stream = &s.stream;
            if stream.retained_start() > self.cursor.offset
                || !stream.markers_retained_after(self.cursor.marker)
            {
                return Pull::Gap;
            }
            let end = stream.offset();
            let chunk = (self.max_frame - 8) as u64;
            let mut markers = stream.markers_after(self.cursor.marker).peekable();
            let mut budget = PULL_BUDGET;
            loop {
                while let Some(m) = markers.next_if(|m| m.offset <= self.cursor.offset) {
                    let frame = match &m.kind {
                        MarkerKind::Resized { cols, rows } => self.json_frame(
                            FrameType::Resized,
                            &Resized {
                                seq: m.seq,
                                offset: m.offset,
                                cols: *cols,
                                rows: *rows,
                            },
                        ),
                        MarkerKind::Exited { exit } => self.json_frame(
                            FrameType::Exited,
                            &Exited {
                                seq: m.seq,
                                offset: m.offset,
                                exit: exit.clone(),
                            },
                        ),
                    };
                    frames.push(frame);
                    self.cursor.marker = m.seq;
                }
                if self.cursor.offset >= end || budget == 0 {
                    break;
                }
                let limit = markers
                    .peek()
                    .map_or(end, |m| m.offset)
                    .min(end)
                    .min(self.cursor.offset + chunk);
                let bytes = stream
                    .read(self.cursor.offset, (limit - self.cursor.offset) as usize)
                    .expect("offset within the retained stream");
                budget = budget.saturating_sub(bytes.len());
                frames.push(encode_frame(
                    FrameType::Output,
                    self.channel,
                    &encode_output(self.cursor.offset, &bytes),
                ));
                self.cursor.offset += bytes.len() as u64;
            }
            self.done = s.status != Status::Running && self.cursor.marker == stream.latest_seq();
        }
        if frames.is_empty() {
            return Pull::Idle;
        }
        for frame in frames {
            self.writer.send(frame);
        }
        Pull::Sent
    }

    fn json_frame<T: serde::Serialize>(&self, frame_type: FrameType, body: &T) -> Vec<u8> {
        crate::frame_json(frame_type, self.channel, body)
    }
}

/// Whether `session` is still running, for `INPUT` and `VIEWPORT`.
pub fn running(session: &Session) -> Result<(), (ErrorCode, &'static str)> {
    if session.status == Status::Running {
        Ok(())
    } else {
        Err((ErrorCode::NotRunning, "session is not running"))
    }
}
