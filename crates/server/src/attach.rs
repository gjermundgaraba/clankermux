//! One attachment: its bootstrap, the stream from its cursor, and its end.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use ghostty_vt::snapshot;
use proto::{
    AttachMode, Attached, ClientPeer, Closed, ClosedReason, ErrorCode, Event, Exited, FrameType,
    Gap, OUTPUT_OFFSET_LEN, Open, Peer, Resized, Resume, Status, Viewport, encode_frame,
    encode_json_frame, encode_output_frame,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::connection::{Writer, error_frame};
use crate::registry::{Fail, Registry};
use crate::session::{
    Attachment, Body, SNAPSHOT_RETRY, Session, Shared, Source, now, printable, snapshot_with_retry,
    truncate,
};
use crate::stream::{Cursor, MarkerKind, Served};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Bytes kept of the client's name and of its version.
const CLIENT_TEXT: usize = 32;

/// Bytes pulled from the stream per lock hold.
const PULL_BUDGET: usize = 64 * 1024;

/// Only output following a recent send waits; the first after idle is immediate.
const OUTPUT_COALESCE: Duration = Duration::from_millis(1);

fn coalesce_until(
    last: Option<Instant>,
    now: Instant,
    bytes: usize,
    marker: bool,
) -> Option<Instant> {
    let until = last? + OUTPUT_COALESCE;
    (bytes > 0 && bytes < PULL_BUDGET && !marker && now < until).then_some(until)
}

/// How an attachment ends, as the session or the connection decides it.
/// The task sends the frame after whatever it had queued.
pub enum Ending {
    Closed(Closed),
    Error(ErrorCode, String),
}

/// The connection's side of an attachment. The task holds the session;
/// the handle only reaches it while the task runs.
pub struct Handle {
    /// Unique on the server: `AttachmentInfo.channel`.
    pub id: u64,
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
        let _ = self.close.send(Ending::Closed(Closed {
            reason: ClosedReason::Closed,
            message: None,
        }));
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

/// Starts serving `open` on `channel` for a connection whose `HELLO`
/// named `client`. The checks that precede this (channel, session,
/// engine, size) are the connection's; a session at
/// `limits.attachments` is `limit_exceeded`.
pub fn open(
    registry: &Arc<Registry>,
    writer: &Writer,
    channel: u32,
    client: &ClientPeer,
    open: Open,
    session: Shared,
) -> Result<Handle, Fail> {
    let limits = &registry.config.limits;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (close_tx, close_rx) = mpsc::unbounded_channel();
    let finished = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    let (events, snapshot) = {
        let mut s = session.lock().unwrap();
        if s.attached.len() as u64 >= limits.attachments {
            return Err((ErrorCode::LimitExceeded, "too many attachments".into()));
        }
        s.attached.push(Attachment {
            id,
            client: Peer {
                name: client_text(&client.name),
                version: client_text(&client.version),
            },
            read_only: open.read_only,
            viewport: None,
            since: now(),
            last_input: None,
            close: close_tx.downgrade(),
            finished: finished.clone(),
        });
        // The same channel and client as the attachments list.
        let opened = Event::AttachmentOpened {
            id: s.id.clone(),
            channel: id,
            client: s.attached.last().expect("just pushed").client.clone(),
            read_only: open.read_only,
        };
        s.events.push(opened);
        // A writable OPEN's viewport is an immediate report; a profile
        // alone is applied as it is.
        if !open.read_only && s.running().is_ok() {
            match open.viewport {
                Some(size) => {
                    s.record_viewport(id, &size);
                    s.apply_report(
                        Viewport {
                            size,
                            profile: open.profile,
                        },
                        Source::Attachment(id),
                    );
                }
                None => {
                    if let Some(profile) = open.profile {
                        s.set_profile(profile);
                    }
                }
            }
        }
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
        last_output: None,
        done: false,
    };
    Ok(Handle {
        id,
        read_only: open.read_only,
        session: Arc::downgrade(&session),
        close: close_tx,
        ended,
        task: registry.attachments.spawn(task.run(open.resume)),
    })
}

/// The first `CLIENT_TEXT` bytes of a client's `HELLO` text, printable,
/// so that the attachments list stays small within `inspect`.
fn client_text(text: &str) -> String {
    truncate(printable(text), CLIENT_TEXT)
}

/// Snapshot bytes not yet sent.
struct PendingSnapshot {
    bytes: Vec<u8>,
    sent: usize,
    /// Live bytes sent since history last got a turn.
    live_bytes: usize,
    /// The bootstrap followed a gap; another gap before it completes is
    /// `too_slow`.
    after_gap: bool,
}

/// The frame that ends an attachment; `None` when the connection dropped
/// the handle, which ends it silently.
type End = Option<Ending>;

enum Pull {
    Sent,
    Wait(Instant),
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
    last_output: Option<Instant>,
    /// The final marker has been sent.
    done: bool,
}

impl Task {
    async fn run(mut self, resume: Option<Resume>) {
        let end = self.serve(resume).await;
        // Detach before the ending frame goes out, so a client that reads
        // it finds the channel free and the attachment gone.
        let (events, snapshot, last) = {
            let mut s = self.session.lock().unwrap();
            s.remove_attachment(self.id);
            self.ended.store(true, Ordering::Release);
            let last = s.attached.is_empty() && s.status() == Status::Running;
            (s.take_events(), s.to_proto(false), last)
        };
        self.registry.emit_all(events);
        // The last detach parks: what the user left is on disk.
        if last {
            let registry = self.registry.clone();
            let session = self.session.clone();
            tokio::spawn(async move { registry.park(&session).await });
        }
        let removed = matches!(
            &end,
            Some(Ending::Closed(Closed {
                reason: ClosedReason::Removed,
                ..
            }))
        );
        let reason = match &end {
            Some(Ending::Closed(closed)) => closed.reason.to_string(),
            Some(Ending::Error(code, _)) => code.to_string(),
            None => "disconnected".into(),
        };
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
        self.registry.emit(Event::AttachmentClosed {
            id: snapshot.id.clone(),
            channel: self.id,
            reason,
        });
        if !removed {
            self.registry
                .emit(Event::SessionChanged { session: snapshot });
        }
    }

    fn send_ending(&self, ending: Ending) {
        match ending {
            Ending::Closed(closed) => self.send_json(FrameType::Closed, &closed),
            Ending::Error(code, message) => {
                self.writer.send(error_frame(self.channel, code, message));
            }
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
            if self
                .snapshot
                .as_ref()
                .is_some_and(|p| p.live_bytes >= PULL_BUDGET)
            {
                self.send_snapshot_chunk();
                continue;
            }
            match self.pull() {
                Pull::Sent => {
                    if self.writer.is_closed() {
                        return None;
                    }
                }
                Pull::Wait(until) => {
                    tokio::select! {
                        ending = self.close.recv() => return ending,
                        _ = notified => {},
                        _ = tokio::time::sleep_until(until) => {}
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
        let cursor = Cursor {
            offset: resume.offset,
            marker: resume.marker,
        };
        Ok(live.stream.retains(cursor).then_some(cursor))
    }

    /// Takes a snapshot, retrying while the continuation limit is exceeded,
    /// then sends `GAP` (after a gap), `ATTACHED`, the snapshot through
    /// `READY` and `SNAPSHOT_READY`. The rest of the snapshot is sent as
    /// the stream allows. A parked session streams its park file, nothing
    /// after.
    async fn bootstrap(&mut self, gap_from: Option<u64>) -> Result<(), End> {
        self.done = false;
        self.last_output = None;
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
            live_bytes: 0,
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
        let end = total.min(pending.sent + self.max_frame.min(PULL_BUDGET));
        let chunk = pending.bytes[pending.sent..end].to_vec();
        pending.sent = end;
        pending.live_bytes = 0;
        let last = end == total;
        if last {
            self.snapshot = None;
            self.note_finished();
        }
        self.send(FrameType::Snapshot, &chunk);
        if last {
            self.send(FrameType::SnapshotEnd, &[]);
        }
        true
    }

    /// Records that the final marker and, if bootstrapping, `SNAPSHOT_END`
    /// are being sent; before they are queued, so a client that has read
    /// them and sends `remove` finds the attachment finished. A `CLOSED`
    /// that follows still comes after them: the task reads its close
    /// channel only once they are queued.
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
            if !stream.retains(self.cursor) {
                return Pull::Gap;
            }
            if let Some(until) = coalesce_until(
                self.last_output,
                Instant::now(),
                (stream.offset() - self.cursor.offset) as usize,
                stream.markers_after(self.cursor.marker).next().is_some(),
            ) {
                return Pull::Wait(until);
            }
            let chunk = self.max_frame - OUTPUT_OFFSET_LEN;
            let mut bytes = 0;
            for served in stream.serve(&mut self.cursor, chunk, PULL_BUDGET) {
                frames.push(match served {
                    Served::Marker(m) => match &m.kind {
                        MarkerKind::Resized { cols, rows } => encode_json_frame(
                            FrameType::Resized,
                            self.channel,
                            &Resized {
                                seq: m.seq,
                                offset: m.offset,
                                cols: *cols,
                                rows: *rows,
                            },
                        ),
                        MarkerKind::Exited { exit } => encode_json_frame(
                            FrameType::Exited,
                            self.channel,
                            &Exited {
                                seq: m.seq,
                                offset: m.offset,
                                exit: exit.clone(),
                            },
                        ),
                    },
                    Served::Output(offset, first, second) => {
                        bytes += first.len() + second.len();
                        encode_output_frame(self.channel, offset, first, second)
                    }
                });
            }
            if bytes > 0 {
                self.last_output = Some(Instant::now());
            }
            if let Some(pending) = &mut self.snapshot {
                pending.live_bytes += bytes;
            }
            self.done = s.status() != Status::Running && self.cursor.marker == stream.latest_seq();
        }
        let sent = !frames.is_empty();
        self.note_finished();
        for frame in frames {
            self.writer.send(frame);
        }
        if sent { Pull::Sent } else { Pull::Idle }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_coalescing_keeps_idle_echo_immediate_and_bounds_bursts() {
        let now = Instant::now();
        assert_eq!(coalesce_until(None, now, 1, false), None);
        assert_eq!(
            coalesce_until(Some(now - OUTPUT_COALESCE), now, 1, false),
            None
        );
        let last = now - OUTPUT_COALESCE / 2;
        assert_eq!(
            coalesce_until(Some(last), now, 1, false),
            Some(last + OUTPUT_COALESCE)
        );
        assert_eq!(coalesce_until(Some(last), now, PULL_BUDGET, false), None);
        assert_eq!(coalesce_until(Some(last), now, 1, true), None);
        assert_eq!(coalesce_until(Some(last), now, 0, false), None);
    }
}
