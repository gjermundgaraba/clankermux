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
use crate::stream::MarkerKind;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

const CLIENT_TEXT: usize = 32;

const PULL_BUDGET: usize = 64 * 1024;

const OUTPUT_COALESCE: Duration = Duration::from_millis(1);

fn coalesce_until(
    last: Option<Instant>,
    now: Instant,
    bytes: u64,
    marker: bool,
) -> Option<Instant> {
    let until = last? + OUTPUT_COALESCE;
    (bytes > 0 && bytes < PULL_BUDGET as u64 && !marker && now < until).then_some(until)
}

pub enum Ending {
    Closed(Closed),
    Error(ErrorCode, String),
}

pub struct Gone;

pub struct Handle {
    pub id: u64,
    pub read_only: bool,
    session: Weak<std::sync::Mutex<Session>>,
    close: mpsc::UnboundedSender<Ending>,
    ended: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    Open,
    Ending,
    Finished,
}

impl Handle {
    pub fn state(&self) -> ChannelState {
        if self.task.is_finished() {
            ChannelState::Finished
        } else if self.ended.load(Ordering::Acquire) {
            ChannelState::Ending
        } else {
            ChannelState::Open
        }
    }

    pub fn session(&self) -> Option<Shared> {
        self.session.upgrade()
    }

    pub fn close(&self) {
        let _ = self.close.send(Ending::Closed(Closed {
            reason: ClosedReason::Closed,
            message: None,
        }));
    }

    pub fn end(&self, code: ErrorCode, message: String) -> Result<(), Gone> {
        self.close
            .send(Ending::Error(code, message))
            .map_err(|_| Gone)
    }

    pub async fn join(self) {
        let _ = self.task.await;
    }
}

pub fn open(
    registry: &Arc<Registry>,
    writer: &Writer,
    channel: u32,
    client: &ClientPeer,
    open: Open,
    session: &Shared,
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
        let opened = Event::AttachmentOpened {
            id: s.id.clone(),
            channel: id,
            client: s.attached.last().expect("just pushed").client.clone(),
            read_only: open.read_only,
        };
        s.events.push(opened);
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
                        s.set_profile(&profile);
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
        max_frame: proto::cap(registry.config.limits.max_frame),
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
        session: Arc::downgrade(session),
        close: close_tx,
        ended,
        task: registry.attachments.spawn(task.run(open.resume)),
    })
}

fn client_text(text: &str) -> String {
    truncate(printable(text), CLIENT_TEXT)
}

#[derive(Clone, Copy)]
struct Cursor {
    offset: u64,
    marker: u64,
}

struct PendingSnapshot {
    bytes: Vec<u8>,
    sent: usize,
    live_bytes: usize,
    after_gap: bool,
}

enum End {
    Disconnected,
    Frame(Ending),
}

impl End {
    fn received(ending: Option<Ending>) -> Self {
        ending.map_or(End::Disconnected, End::Frame)
    }
}

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
    done: bool,
}

impl Task {
    async fn run(mut self, resume: Option<Resume>) {
        let end = self.serve(resume).await;
        let (events, snapshot, last) = {
            let mut s = self.session.lock().unwrap();
            s.remove_attachment(self.id);
            self.ended.store(true, Ordering::Release);
            let last = s.attached.is_empty() && s.status() == Status::Running;
            (s.take_events(), s.to_proto(false), last)
        };
        self.registry.emit_all(events);
        if last {
            let registry = self.registry.clone();
            let session = self.session.clone();
            tokio::spawn(async move { registry.park(&session).await });
        }
        let removed = matches!(
            &end,
            End::Frame(Ending::Closed(Closed {
                reason: ClosedReason::Removed,
                ..
            }))
        );
        let reason = match &end {
            End::Frame(Ending::Closed(closed)) => closed.reason.to_string(),
            End::Frame(Ending::Error(code, _)) => code.to_string(),
            End::Disconnected => "disconnected".into(),
        };
        if let End::Frame(ending) = end {
            self.send_ending(ending);
        }
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
            match self.close.try_recv() {
                Ok(ending) => return End::Frame(ending),
                Err(mpsc::error::TryRecvError::Disconnected) => return End::Disconnected,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            let notified = wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.writer.over_high_water() {
                tokio::select! {
                    ending = self.close.recv() => return End::received(ending),
                    () = self.writer.drained() => {}
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
                        return End::Disconnected;
                    }
                }
                Pull::Wait(until) => {
                    tokio::select! {
                        ending = self.close.recv() => return End::received(ending),
                        () = notified => {},
                        () = tokio::time::sleep_until(until) => {}
                    }
                }
                Pull::Gap => {
                    if self.snapshot.as_ref().is_some_and(|p| p.after_gap) {
                        return End::Frame(Ending::Error(
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
                        ending = self.close.recv() => return End::received(ending),
                        () = notified => {}
                    }
                }
            }
        }
    }

    fn resume_cursor(&self, resume: Option<Resume>) -> Result<Option<Cursor>, End> {
        let Some(resume) = resume.filter(|r| r.incarnation == self.registry.incarnation) else {
            return Ok(None);
        };
        let s = self.session.lock().unwrap();
        let above_latest = || {
            Err(End::Frame(Ending::Error(
                ErrorCode::InvalidRequest,
                "resume marker above the latest".into(),
            )))
        };
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

    async fn bootstrap(&mut self, gap_from: Option<u64>) -> Result<(), End> {
        self.done = false;
        self.last_output = None;
        let internal = |message: String| End::Frame(Ending::Error(ErrorCode::Internal, message));
        let parked_path = {
            let s = self.session.lock().unwrap();
            s.parked().map(|p| p.snapshot_path.clone())
        };
        let (bytes, cursor, session) = if let Some(path) = parked_path {
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
        } else {
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
                ending = self.close.recv() => return Err(End::received(ending)),
            };
            match taken {
                Ok(Some(taken)) => taken,
                Ok(None) => return Err(internal("session is no longer live".into())),
                Err(err) => {
                    let code = match err {
                        ghostty_vt::Error::LimitExceeded => ErrorCode::LimitExceeded,
                        _ => ErrorCode::Internal,
                    };
                    return Err(End::Frame(Ending::Error(
                        code,
                        format!("snapshot failed: {err}"),
                    )));
                }
            }
        };
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
        for chunk in bytes[..ready].chunks(self.max_frame) {
            if self.writer.over_high_water() {
                tokio::select! {
                    ending = self.close.recv() => return Err(End::received(ending)),
                    () = self.writer.drained() => {}
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

    fn note_finished(&self) {
        if self.done && self.snapshot.is_none() {
            self.finished.store(true, Ordering::Release);
        }
    }

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
            let chunk = self.max_frame - OUTPUT_OFFSET_LEN;
            let mut markers = stream.markers_after(self.cursor.marker).peekable();
            if let Some(until) = coalesce_until(
                self.last_output,
                Instant::now(),
                end - self.cursor.offset,
                markers.peek().is_some(),
            ) {
                return Pull::Wait(until);
            }
            let mut budget = PULL_BUDGET;
            loop {
                while let Some(m) = markers.next_if(|m| m.offset <= self.cursor.offset) {
                    let frame = match &m.kind {
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
                let (first, second) = stream
                    .read_slices(self.cursor.offset, limit)
                    .expect("offset within the retained stream");
                let len = first.len() + second.len();
                budget -= len;
                frames.push(encode_output_frame(
                    self.channel,
                    self.cursor.offset,
                    first,
                    second,
                ));
                self.cursor.offset += len as u64;
            }
            if budget < PULL_BUDGET {
                self.last_output = Some(Instant::now());
            }
            if let Some(pending) = &mut self.snapshot {
                pending.live_bytes += PULL_BUDGET - budget;
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
        assert_eq!(
            coalesce_until(Some(last), now, PULL_BUDGET as u64, false),
            None
        );
        assert_eq!(coalesce_until(Some(last), now, 1, true), None);
        assert_eq!(coalesce_until(Some(last), now, 0, false), None);
    }
}
