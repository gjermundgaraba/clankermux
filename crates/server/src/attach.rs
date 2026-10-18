//! One attachment: its bootstrap, the stream from its cursor, and its end.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use ghostty_vt::snapshot;
use proto::{
    AttachMode, Attached, Closed, ClosedReason, ErrorCode, ErrorMessage, Event, Exited, FrameType,
    Gap, Open, Resized, Resume, Status, Viewport, encode_frame, encode_output,
};
use tokio::sync::mpsc;

use crate::connection::Writer;
use crate::registry::Registry;
use crate::session::{Attachment, Body, SNAPSHOT_RETRY, Session, Shared, snapshot_with_retry};
use crate::stream::MarkerKind;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Bytes pulled from the stream per lock hold.
const PULL_BUDGET: usize = 64 * 1024;

/// How an attachment ends, as the session or the connection decides it.
/// The task sends the frame after whatever it had queued.
pub enum Ending {
    Closed(ClosedReason),
    Error(ErrorCode, String),
}

/// The connection's side of an attachment. The task holds the session;
/// the handle only reaches it while the task runs.
pub struct Handle {
    pub read_only: bool,
    session: Weak<std::sync::Mutex<Session>>,
    close: mpsc::UnboundedSender<Ending>,
    /// Set by the task once it has detached and will send nothing but
    /// its ending.
    ended: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Handle {
    /// Whether the attachment still takes frames; a channel is free again
    /// only once the task has [`Handle::finished`].
    pub fn is_open(&self) -> bool {
        !self.ended.load(Ordering::Acquire)
    }

    /// Whether the task has queued its last frame and returned.
    pub fn finished(&self) -> bool {
        self.task.is_finished()
    }

    pub fn session(&self) -> Option<Shared> {
        self.session.upgrade()
    }

    /// `CLOSE`: the task sends `CLOSED closed` after its pending frames.
    pub fn close(&self) {
        let _ = self.close.send(Ending::Closed(ClosedReason::Closed));
    }

    /// Asks the task to end with `ERROR code` after its pending frames.
    /// False when the task is gone: it sends nothing more, and the caller
    /// sends the error itself.
    pub fn end(&self, code: ErrorCode, message: String) -> bool {
        self.close.send(Ending::Error(code, message)).is_ok()
    }

    /// Waits for the task, which has queued every frame of the
    /// attachment by then.
    pub async fn join(self) {
        let _ = self.task.await;
    }
}

/// Starts serving `open` on `channel`. The checks that precede this
/// (channel, session, engine, size) are the connection's.
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
    let ended = Arc::new(AtomicBool::new(false));
    let (events, snapshot) = {
        let mut s = session.lock().unwrap();
        // A writable OPEN's viewport and profile are one immediate report.
        if !open.read_only && (open.viewport.is_some() || open.profile.is_some()) {
            let size = open.viewport.unwrap_or_else(|| s.size());
            s.apply_report(Viewport {
                size,
                profile: open.profile,
            });
        }
        s.attached.push(Attachment {
            id,
            close: close_tx.downgrade(),
            finished: finished.clone(),
        });
        (s.take_events(), s.to_proto(false))
    };
    registry.emit_session(events, Some(snapshot));
    let task = Task {
        id,
        channel,
        writer: writer.clone(),
        registry: registry.clone(),
        session: session.clone(),
        close: close_rx,
        finished,
        ended: ended.clone(),
        max_frame: registry.config.limits.max_frame as usize,
        cursor: Cursor {
            offset: 0,
            marker: 0,
        },
        snapshot: None,
        done: false,
    };
    Handle {
        read_only: open.read_only,
        session: Arc::downgrade(&session),
        close: close_tx,
        ended,
        task: registry.attachments.spawn(task.run(open.resume)),
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
    /// The bootstrap followed a gap; another gap before it completes is
    /// `too_slow`.
    after_gap: bool,
}

/// The frame that ends an attachment; `None` when the connection dropped
/// the handle, which ends it silently.
type End = Option<Ending>;

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
    close: mpsc::UnboundedReceiver<Ending>,
    finished: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    max_frame: usize,
    cursor: Cursor,
    snapshot: Option<PendingSnapshot>,
    /// The final marker has been sent.
    done: bool,
}

impl Task {
    async fn run(mut self, resume: Option<Resume>) {
        let end = self.serve(resume).await;
        // Detach before the ending frame goes out, so a client that reads
        // it finds the channel free and the attachment gone.
        let snapshot = {
            let mut s = self.session.lock().unwrap();
            s.attached.retain(|a| a.id != self.id);
            self.ended.store(true, Ordering::Release);
            s.to_proto(false)
        };
        let removed = matches!(end, Some(Ending::Closed(ClosedReason::Removed)));
        if let Some(ending) = end {
            self.send_ending(ending);
        }
        // An ending the connection queued meanwhile (an `OPEN` on this
        // channel before it read the `CLOSED`) follows in order; closing
        // the channel first makes a later one fail, so the connection
        // sends it after this task's frames.
        self.close.close();
        while let Ok(ending) = self.close.try_recv() {
            if let Ending::Error(..) = ending {
                self.send_ending(ending);
            }
        }
        if !removed {
            self.registry
                .emit(&Event::SessionChanged { session: snapshot });
        }
    }

    fn send_ending(&self, ending: Ending) {
        match ending {
            Ending::Closed(reason) => self.send_json(FrameType::Closed, &Closed { reason }),
            Ending::Error(code, message) => self.send_json(
                FrameType::Error,
                &ErrorMessage {
                    id: None,
                    code,
                    message,
                },
            ),
        }
    }

    fn send(&self, frame_type: FrameType, payload: &[u8]) {
        self.writer
            .send(encode_frame(frame_type, self.channel, payload));
    }

    fn send_json<T: serde::Serialize>(&self, frame_type: FrameType, body: &T) {
        self.writer.send_json(frame_type, self.channel, body);
    }

    async fn serve(&mut self, resume: Option<Resume>) -> End {
        match self.resume_cursor(resume) {
            Err(end) => return end,
            Ok(Some(cursor)) => {
                let session = self.session.lock().unwrap().to_proto(false);
                self.cursor = cursor;
                self.send_json(
                    FrameType::Attached,
                    &Attached {
                        mode: AttachMode::Resume,
                        offset: cursor.offset,
                        marker: cursor.marker,
                        cols: session.cols,
                        rows: session.rows,
                        session,
                        snapshot_bytes: None,
                    },
                );
            }
            Ok(None) => {
                if let Err(end) = self.bootstrap(None).await {
                    return end;
                }
            }
        }
        let wake = self.session.lock().unwrap().wake.clone();
        loop {
            // A close is observed every iteration, not only when idle.
            match self.close.try_recv() {
                Ok(ending) => return Some(ending),
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            let notified = wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.writer.over_high_water() {
                tokio::select! {
                    ending = self.close.recv() => return ending,
                    _ = self.writer.drained() => {}
                }
                continue;
            }
            match self.pull() {
                Pull::Sent => {
                    if self.writer.is_closed() {
                        return None;
                    }
                }
                Pull::Gap => {
                    if self.snapshot.as_ref().is_some_and(|p| p.after_gap) {
                        return Some(Ending::Error(
                            ErrorCode::TooSlow,
                            "fell behind again during a bootstrap".into(),
                        ));
                    }
                    if let Err(end) = self.bootstrap(Some(self.cursor.offset)).await {
                        return end;
                    }
                }
                Pull::Idle => {
                    if self.send_snapshot_chunk() {
                        continue;
                    }
                    tokio::select! {
                        ending = self.close.recv() => return ending,
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
        let above_latest = || {
            Err(Some(Ending::Error(
                ErrorCode::InvalidRequest,
                "resume marker above the latest".into(),
            )))
        };
        // A parked session has no stream to resume on.
        let live = match &s.body {
            Body::Live(live) => live,
            Body::Parked(parked) if resume.marker > parked.marker => {
                return above_latest();
            }
            Body::Parked(_) => return Ok(None),
        };
        if resume.marker > live.stream.latest_seq() {
            return above_latest();
        }
        let retained = live.stream.retained_start() <= resume.offset
            && resume.offset <= live.stream.offset()
            && live.stream.markers_retained_after(resume.marker);
        Ok(retained.then_some(Cursor {
            offset: resume.offset,
            marker: resume.marker,
        }))
    }

    /// Takes a snapshot, retrying while the continuation limit is exceeded,
    /// then sends `GAP` (after a gap), `ATTACHED`, the snapshot through
    /// `READY` and `SNAPSHOT_READY`. The rest of the snapshot is sent as
    /// the stream allows. A parked session streams its park file, nothing
    /// after.
    async fn bootstrap(&mut self, gap_from: Option<u64>) -> Result<(), End> {
        self.done = false;
        let internal = |message: String| Some(Ending::Error(ErrorCode::Internal, message));
        let parked_path = {
            let s = self.session.lock().unwrap();
            s.parked().map(|p| p.snapshot_path.clone())
        };
        let (bytes, cursor, session) = match parked_path {
            Some(path) => {
                let bytes = tokio::task::spawn_blocking(move || std::fs::read(path))
                    .await
                    .map_err(|e| internal(e.to_string()))?
                    .map_err(|e| internal(format!("park file unreadable: {e}")))?;
                self.done = true;
                let s = self.session.lock().unwrap();
                let Some(parked) = s.parked() else {
                    return Err(internal("session changed while reading its park".into()));
                };
                (
                    bytes,
                    Cursor {
                        offset: parked.offset,
                        marker: parked.marker,
                    },
                    s.to_proto(false),
                )
            }
            None => {
                let taken = tokio::select! {
                    taken = snapshot_with_retry(&self.session, SNAPSHOT_RETRY, |s, live, bytes| {
                        (
                            bytes,
                            Cursor {
                                offset: live.stream.offset(),
                                marker: live.stream.latest_seq(),
                            },
                            s.to_proto(false),
                        )
                    }) => taken,
                    ending = self.close.recv() => return Err(ending),
                };
                match taken {
                    Ok(Some(taken)) => taken,
                    Ok(None) => return Err(internal("session is no longer live".into())),
                    Err(err) => {
                        let code = match err {
                            ghostty_vt::Error::LimitExceeded => ErrorCode::LimitExceeded,
                            _ => ErrorCode::Internal,
                        };
                        return Err(Some(Ending::Error(code, format!("snapshot failed: {err}"))));
                    }
                }
            }
        };
        // A file from disk is checked before anything is promised.
        let Some(ready) = snapshot::ready_end(&bytes) else {
            return Err(internal("snapshot has no READY".into()));
        };
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
        self.send_json(
            FrameType::Attached,
            &Attached {
                mode: AttachMode::Snapshot,
                offset: cursor.offset,
                marker: cursor.marker,
                cols: session.cols,
                rows: session.rows,
                session,
                snapshot_bytes: Some(bytes.len() as u64),
            },
        );
        // Paced like the rest of the snapshot: a prefix can hold the whole
        // continuation, more than the writer's ceiling.
        for chunk in bytes[..ready].chunks(self.max_frame) {
            if self.writer.over_high_water() {
                tokio::select! {
                    ending = self.close.recv() => return Err(ending),
                    _ = self.writer.drained() => {}
                }
            }
            self.send(FrameType::Snapshot, chunk);
        }
        self.send(FrameType::SnapshotReady, &[]);
        self.snapshot = Some(PendingSnapshot {
            bytes,
            sent: ready,
            after_gap: gap_from.is_some(),
        });
        Ok(())
    }

    /// Sends the next remaining snapshot chunk, and `SNAPSHOT_END` after
    /// the last. False when no snapshot is pending.
    fn send_snapshot_chunk(&mut self) -> bool {
        let Some(pending) = self.snapshot.as_mut() else {
            return false;
        };
        let total = pending.bytes.len();
        let end = total.min(pending.sent + self.max_frame);
        let chunk = pending.bytes[pending.sent..end].to_vec();
        pending.sent = end;
        self.send(FrameType::Snapshot, &chunk);
        if end == total {
            self.snapshot = None;
            self.send(FrameType::SnapshotEnd, &[]);
            self.note_finished();
        }
        true
    }

    /// Records that the final marker and, if bootstrapping, `SNAPSHOT_END`
    /// have been sent.
    fn note_finished(&self) {
        if self.done && self.snapshot.is_none() {
            self.finished.store(true, Ordering::Release);
        }
    }

    /// Sends the stream from the cursor: each marker before any byte at or
    /// past its offset, up to `PULL_BUDGET` bytes.
    fn pull(&mut self) -> Pull {
        let mut frames = Vec::new();
        {
            let s = self.session.lock().unwrap();
            let Some(live) = s.live() else {
                return Pull::Idle;
            };
            let stream = &live.stream;
            if stream.retained_start() > self.cursor.offset
                || !stream.markers_retained_after(self.cursor.marker)
            {
                return Pull::Gap;
            }
            let end = stream.offset();
            let chunk = self.max_frame - 8;
            let mut markers = stream.markers_after(self.cursor.marker).peekable();
            let mut budget = PULL_BUDGET;
            loop {
                while let Some(m) = markers.next_if(|m| m.offset <= self.cursor.offset) {
                    let frame = match &m.kind {
                        MarkerKind::Resized { cols, rows } => crate::frame_json(
                            FrameType::Resized,
                            self.channel,
                            &Resized {
                                seq: m.seq,
                                offset: m.offset,
                                cols: *cols,
                                rows: *rows,
                            },
                        ),
                        MarkerKind::Exited { exit } => crate::frame_json(
                            FrameType::Exited,
                            self.channel,
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
                    .min(self.cursor.offset + chunk.min(budget) as u64);
                let bytes = stream
                    .read(self.cursor.offset, (limit - self.cursor.offset) as usize)
                    .expect("offset within the retained stream");
                budget -= bytes.len();
                frames.push(encode_frame(
                    FrameType::Output,
                    self.channel,
                    &encode_output(self.cursor.offset, &bytes),
                ));
                self.cursor.offset += bytes.len() as u64;
            }
            self.done = s.status() != Status::Running && self.cursor.marker == stream.latest_seq();
        }
        let sent = !frames.is_empty();
        for frame in frames {
            self.writer.send(frame);
        }
        self.note_finished();
        if sent { Pull::Sent } else { Pull::Idle }
    }
}
