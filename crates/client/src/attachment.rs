//! The attachment state machine: frames in, actions out. It keeps the
//! local engine current (bootstrapping from the snapshot, then feeding
//! the stream) and tracks the cursor for resuming.

use ghostty_vt::snapshot::Decoder;
use ghostty_vt::{Handler, Terminal};
use proto::{
    AttachMode, Attached, Closed, ClosedReason, ErrorMessage, Exit, Exited, FrameType, Gap,
    Resized, Resume, Session, Welcome, parse_output,
};

use crate::connection::{Error, Frame};

/// What a client does in response to frames.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// The screen is renderable from the local engine; paint it. The
    /// session may already have exited: see [`Attachment::session`].
    Screen,
    /// Stream bytes, already applied to the local engine.
    Output(Vec<u8>),
    /// The session was resized at the current position.
    Resized {
        cols: u16,
        rows: u16,
    },
    Exited(Exit),
    /// Bytes were lost; the terminal was discarded and a new bootstrap
    /// follows.
    Gap(Gap),
    Closed(ClosedReason),
    /// `read_only`, `not_running` and `input_overflow` keep the
    /// attachment; any other code ended it.
    Error(ErrorMessage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    offset: u64,
    marker: u64,
}

enum State<H: Handler> {
    /// Before `ATTACHED`.
    Opening,
    Bootstrapping {
        decoder: Decoder<H>,
        /// Stream frames held back after a `RESIZED` or `EXITED` until
        /// `SNAPSHOT_END`.
        held: Vec<Frame>,
    },
    Streaming(Terminal<H>),
    /// After the final marker, `CLOSED` or a fatal error. The terminal,
    /// when there was one, stays renderable for the final screen.
    Ended(Option<Terminal<H>>),
}

/// The attachment of one channel. `H` is the handler installed on the
/// local engine from `READY` on; a fresh `H::default()` per bootstrap.
pub struct Attachment<H: Handler + Default = ()> {
    /// The server the cursor was obtained from, and its limit.
    incarnation: String,
    continuation: usize,
    /// The server of the connection the next `ATTACHED` arrives on.
    next: Option<(String, usize)>,
    state: State<H>,
    cursor: Cursor,
    pub session: Option<Session>,
}

impl<H: Handler + Default> Default for Attachment<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: Handler + Default> Attachment<H> {
    pub fn new() -> Self {
        Attachment {
            incarnation: String::new(),
            continuation: 0,
            next: None,
            state: State::Opening,
            cursor: Cursor {
                offset: 0,
                marker: 0,
            },
            session: None,
        }
    }

    /// Records the server an `OPEN` is about to go to; its incarnation
    /// becomes the cursor's once that `ATTACHED` arrives.
    pub fn open_on(&mut self, welcome: &Welcome) {
        self.next = Some((
            welcome.incarnation.clone(),
            welcome.limits.continuation as usize,
        ));
    }

    /// The cursor to resume from, once the terminal reflects the stream.
    pub fn resume(&self) -> Option<Resume> {
        match self.state {
            State::Streaming(_) => Some(Resume {
                incarnation: self.incarnation.clone(),
                offset: self.cursor.offset,
                marker: self.cursor.marker,
            }),
            _ => None,
        }
    }

    pub fn ended(&self) -> bool {
        matches!(self.state, State::Ended(_))
    }

    /// The local engine, once `READY` has been decoded.
    pub fn term(&mut self) -> Option<&mut Terminal<H>> {
        match &mut self.state {
            State::Bootstrapping { decoder, .. } => decoder.terminal(),
            State::Streaming(term) => Some(term),
            State::Ended(term) => term.as_mut(),
            State::Opening => None,
        }
    }

    /// Ends the attachment, keeping the terminal it had.
    fn end(&mut self) {
        let term = match std::mem::replace(&mut self.state, State::Opening) {
            State::Streaming(term) | State::Ended(Some(term)) => Some(term),
            State::Bootstrapping { decoder, .. } => decoder.into_terminal(),
            State::Opening | State::Ended(None) => None,
        };
        self.state = State::Ended(term);
    }

    pub fn handle(&mut self, frame: Frame) -> Result<Vec<Action>, Error> {
        let mut actions = Vec::new();
        self.step(frame, &mut actions)?;
        Ok(actions)
    }

    fn step(&mut self, frame: Frame, actions: &mut Vec<Action>) -> Result<(), Error> {
        let kind = frame.header.frame_type;
        match kind {
            FrameType::Attached => {
                let attached: Attached = frame.json()?;
                if let Some((incarnation, continuation)) = self.next.take() {
                    self.incarnation = incarnation;
                    self.continuation = continuation;
                }
                self.cursor = Cursor {
                    offset: attached.offset,
                    marker: attached.marker,
                };
                self.session = Some(attached.session);
                match attached.mode {
                    AttachMode::Snapshot => {
                        let mut decoder = Decoder::with_handler(H::default()).map_err(engine)?;
                        decoder
                            .set_max_continuation_bytes(self.continuation)
                            .map_err(engine)?;
                        self.state = State::Bootstrapping {
                            decoder,
                            held: Vec::new(),
                        };
                    }
                    AttachMode::Resume => {
                        let State::Streaming(_) = self.state else {
                            return Err(Error::Protocol("resume without a terminal".into()));
                        };
                    }
                }
            }
            FrameType::Snapshot => {
                let State::Bootstrapping { decoder, .. } = &mut self.state else {
                    return Err(Error::Protocol("SNAPSHOT outside a bootstrap".into()));
                };
                decoder.push(&frame.payload);
                if decoder.terminal().is_none() {
                    if decoder.ready_available() {
                        decoder.ready().map_err(engine)?;
                        actions.push(Action::Screen);
                    }
                } else {
                    while decoder.step_available() && decoder.next_page().map_err(engine)? {}
                }
            }
            FrameType::SnapshotReady => {
                let State::Bootstrapping { decoder, .. } = &mut self.state else {
                    return Err(Error::Protocol("SNAPSHOT_READY outside a bootstrap".into()));
                };
                if decoder.terminal().is_none() {
                    return Err(Error::Protocol("SNAPSHOT_READY before READY".into()));
                }
            }
            FrameType::SnapshotEnd => {
                if !matches!(self.state, State::Bootstrapping { .. }) {
                    return Err(Error::Protocol("SNAPSHOT_END outside a bootstrap".into()));
                }
                let State::Bootstrapping { mut decoder, held } =
                    std::mem::replace(&mut self.state, State::Opening)
                else {
                    unreachable!()
                };
                while decoder.step_available() && decoder.next_page().map_err(engine)? {}
                if !decoder.finished() {
                    return Err(Error::Protocol("SNAPSHOT_END before FINISH".into()));
                }
                let term = decoder
                    .into_terminal()
                    .ok_or_else(|| Error::Protocol("SNAPSHOT_END before READY".into()))?;
                self.state = State::Streaming(term);
                for frame in held {
                    self.step(frame, actions)?;
                }
            }
            FrameType::Output | FrameType::Resized | FrameType::Exited => {
                if let State::Bootstrapping { held, .. } = &mut self.state
                    && (!held.is_empty() || kind != FrameType::Output)
                {
                    held.push(frame);
                    return Ok(());
                }
                if !matches!(
                    self.state,
                    State::Bootstrapping { .. } | State::Streaming(_)
                ) {
                    return Err(Error::Protocol(format!("{kind:?} outside the stream")));
                }
                self.stream(frame, actions)?;
            }
            FrameType::Gap => {
                let gap: Gap = frame.json()?;
                self.state = State::Opening;
                actions.push(Action::Gap(gap));
            }
            FrameType::Closed => {
                let closed: Closed = frame.json()?;
                self.end();
                actions.push(Action::Closed(closed.reason));
            }
            FrameType::Error => {
                let error: ErrorMessage = frame.json()?;
                if !matches!(
                    error.code,
                    proto::ErrorCode::ReadOnly
                        | proto::ErrorCode::NotRunning
                        | proto::ErrorCode::InputOverflow
                ) {
                    self.end();
                }
                actions.push(Action::Error(error));
            }
            other => return Err(Error::Protocol(format!("{other:?} on an attachment"))),
        }
        Ok(())
    }

    /// A stream frame at the cursor.
    fn stream(&mut self, frame: Frame, actions: &mut Vec<Action>) -> Result<(), Error> {
        let cursor = self.cursor;
        match frame.header.frame_type {
            FrameType::Output => {
                let (offset, bytes) = parse_output(&frame.payload)
                    .ok_or_else(|| Error::Protocol("short OUTPUT".into()))?;
                if offset != cursor.offset {
                    return Err(Error::Desync(format!(
                        "OUTPUT at {offset}, expected {}",
                        cursor.offset
                    )));
                }
                let term = self
                    .term()
                    .ok_or_else(|| Error::Protocol("OUTPUT before the terminal exists".into()))?;
                term.vt_write(bytes);
                self.cursor.offset += bytes.len() as u64;
                actions.push(Action::Output(bytes.to_vec()));
            }
            FrameType::Resized => {
                let resized: Resized = frame.json()?;
                self.check_marker(resized.seq, resized.offset)?;
                if let Some(term) = self.term() {
                    term.resize(resized.cols, resized.rows, 0, 0)
                        .map_err(engine)?;
                }
                actions.push(Action::Resized {
                    cols: resized.cols,
                    rows: resized.rows,
                });
            }
            FrameType::Exited => {
                let exited: Exited = frame.json()?;
                self.check_marker(exited.seq, exited.offset)?;
                self.end();
                actions.push(Action::Exited(exited.exit));
            }
            _ => unreachable!("only stream frames reach here"),
        }
        Ok(())
    }

    fn check_marker(&mut self, seq: u64, offset: u64) -> Result<(), Error> {
        if offset != self.cursor.offset || seq != self.cursor.marker + 1 {
            return Err(Error::Desync(format!(
                "marker {seq} at {offset}, expected {} at {}",
                self.cursor.marker + 1,
                self.cursor.offset
            )));
        }
        self.cursor.marker = seq;
        Ok(())
    }
}

fn engine(err: ghostty_vt::Error) -> Error {
    Error::Protocol(format!("engine: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostty_vt::snapshot;
    use proto::{Header, Limits, Status};

    fn frame(frame_type: FrameType, payload: Vec<u8>) -> Frame {
        Frame {
            header: Header {
                frame_type,
                channel: 1,
                length: payload.len() as u32,
            },
            payload,
        }
    }

    fn json(frame_type: FrameType, body: &impl serde::Serialize) -> Frame {
        frame(frame_type, serde_json::to_vec(body).unwrap())
    }

    fn welcome(incarnation: &str) -> Welcome {
        Welcome {
            version: 1,
            server: proto::Peer {
                name: "test".into(),
                version: "0".into(),
            },
            engine: ghostty_vt::ENGINE.into(),
            incarnation: incarnation.into(),
            capabilities: vec![],
            limits: Limits::default(),
        }
    }

    fn session(cols: u16, rows: u16) -> Session {
        Session {
            id: "s".into(),
            label: String::new(),
            status: Status::Running,
            argv: vec![],
            cwd: String::new(),
            pid: None,
            cols,
            rows,
            title: String::new(),
            pwd: String::new(),
            on_exit: proto::OnExit::Keep,
            created_at: String::new(),
            exited_at: None,
            exit: None,
            offset: 0,
            marker: 0,
            attachments: 1,
            program_status: None,
        }
    }

    fn attached(offset: u64, marker: u64, bytes: &[u8]) -> Frame {
        json(
            FrameType::Attached,
            &Attached {
                mode: AttachMode::Snapshot,
                offset,
                marker,
                cols: 10,
                rows: 3,
                session: session(10, 3),
                snapshot_bytes: Some(bytes.len() as u64),
            },
        )
    }

    fn text(term: &Terminal) -> String {
        let out = ghostty_vt::formatter::format(
            term,
            ghostty_vt::formatter::Options {
                trim: true,
                ..Default::default()
            },
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn snapshot_of(lines: &[u8]) -> Vec<u8> {
        let mut source = Terminal::new(10, 3).unwrap();
        source.vt_write(lines);
        snapshot::encode(&source).unwrap()
    }

    #[test]
    fn bootstraps_holds_after_resize_and_resumes() {
        let bytes = snapshot_of(b"one\r\ntwo\r\n");
        let ready = snapshot::ready_end(&bytes).unwrap();

        let mut a: Attachment = Attachment::new();
        a.open_on(&welcome("inc"));
        assert_eq!(a.handle(attached(100, 2, &bytes)).unwrap(), vec![]);
        // The snapshot arrives in two pieces: the READY part split oddly,
        // then the rest.
        assert_eq!(
            a.handle(frame(FrameType::Snapshot, bytes[..ready - 1].to_vec()))
                .unwrap(),
            vec![]
        );
        assert_eq!(
            a.handle(frame(FrameType::Snapshot, bytes[ready - 1..ready].to_vec()))
                .unwrap(),
            vec![Action::Screen]
        );
        assert_eq!(
            a.handle(frame(FrameType::SnapshotReady, vec![])).unwrap(),
            vec![]
        );
        assert_eq!(text(a.term().unwrap()), "one\ntwo");
        // Output before SNAPSHOT_END is applied at once; after a RESIZED
        // it is held until SNAPSHOT_END.
        let out = proto::encode_output(100, b"x");
        assert_eq!(
            a.handle(frame(FrameType::Output, out)).unwrap(),
            vec![Action::Output(b"x".to_vec())]
        );
        let resized = Resized {
            seq: 3,
            offset: 101,
            cols: 20,
            rows: 5,
        };
        assert_eq!(
            a.handle(json(FrameType::Resized, &resized)).unwrap(),
            vec![]
        );
        assert_eq!(
            a.handle(frame(FrameType::Output, proto::encode_output(101, b"y")))
                .unwrap(),
            vec![]
        );
        assert_eq!(
            a.handle(frame(FrameType::Snapshot, bytes[ready..].to_vec()))
                .unwrap(),
            vec![]
        );
        assert_eq!(
            a.handle(frame(FrameType::SnapshotEnd, vec![])).unwrap(),
            vec![
                Action::Resized { cols: 20, rows: 5 },
                Action::Output(b"y".to_vec())
            ]
        );
        assert_eq!(text(a.term().unwrap()), "one\ntwo\nxy");
        assert_eq!(a.term().unwrap().cols(), 20);
        assert_eq!(
            a.resume(),
            Some(Resume {
                incarnation: "inc".into(),
                offset: 102,
                marker: 3
            })
        );

        // A wrong offset is a client-side error.
        assert!(
            a.handle(frame(FrameType::Output, proto::encode_output(5, b"z")))
                .is_err()
        );

        let exited = Exited {
            seq: 4,
            offset: 102,
            exit: Exit::Code { code: 0 },
        };
        assert_eq!(
            a.handle(json(FrameType::Exited, &exited)).unwrap(),
            vec![Action::Exited(Exit::Code { code: 0 })]
        );
        assert!(a.ended());
        assert_eq!(a.resume(), None);
    }

    #[test]
    fn exited_during_a_bootstrap_waits_for_snapshot_end() {
        let bytes = snapshot_of(b"one\r\ntwo\r\n");
        let ready = snapshot::ready_end(&bytes).unwrap();
        let mut a: Attachment = Attachment::new();
        a.open_on(&welcome("inc"));
        a.handle(attached(100, 0, &bytes)).unwrap();
        assert_eq!(
            a.handle(frame(FrameType::Snapshot, bytes[..ready].to_vec()))
                .unwrap(),
            vec![Action::Screen]
        );
        a.handle(frame(FrameType::SnapshotReady, vec![])).unwrap();
        let exited = Exited {
            seq: 1,
            offset: 100,
            exit: Exit::Code { code: 3 },
        };
        assert_eq!(a.handle(json(FrameType::Exited, &exited)).unwrap(), vec![]);
        assert!(!a.ended(), "held until the history has arrived");
        a.handle(frame(FrameType::Snapshot, bytes[ready..].to_vec()))
            .unwrap();
        assert_eq!(
            a.handle(frame(FrameType::SnapshotEnd, vec![])).unwrap(),
            vec![Action::Exited(Exit::Code { code: 3 })]
        );
        assert!(a.ended());
        // Nothing more is accepted on an ended attachment.
        assert!(a.handle(frame(FrameType::SnapshotEnd, vec![])).is_err());
        assert!(a.ended());
    }

    #[test]
    fn a_snapshot_cut_before_finish_is_a_protocol_error() {
        let bytes = snapshot_of(b"one\r\ntwo\r\n");
        let ready = snapshot::ready_end(&bytes).unwrap();
        let mut a: Attachment = Attachment::new();
        a.open_on(&welcome("inc"));
        a.handle(attached(0, 0, &bytes)).unwrap();
        a.handle(frame(FrameType::Snapshot, bytes[..ready].to_vec()))
            .unwrap();
        a.handle(frame(FrameType::SnapshotReady, vec![])).unwrap();
        a.handle(frame(
            FrameType::Snapshot,
            bytes[ready..bytes.len() - 1].to_vec(),
        ))
        .unwrap();
        assert!(matches!(
            a.handle(frame(FrameType::SnapshotEnd, vec![])),
            Err(Error::Protocol(m)) if m.contains("FINISH")
        ));
    }

    #[test]
    fn the_cursor_keeps_the_incarnation_it_was_obtained_under() {
        let bytes = snapshot_of(b"");
        let mut a: Attachment = Attachment::new();
        a.open_on(&welcome("first"));
        a.handle(attached(7, 0, &bytes)).unwrap();
        a.handle(frame(FrameType::Snapshot, bytes.clone())).unwrap();
        a.handle(frame(FrameType::SnapshotReady, vec![])).unwrap();
        a.handle(frame(FrameType::SnapshotEnd, vec![])).unwrap();
        assert_eq!(a.resume().unwrap().incarnation, "first");
        // Reconnected to a restarted server: the resume offered carries
        // the old incarnation until the new ATTACHED arrives.
        a.open_on(&welcome("second"));
        assert_eq!(a.resume().unwrap().incarnation, "first");
        a.handle(attached(0, 0, &bytes)).unwrap();
        a.handle(frame(FrameType::Snapshot, bytes.clone())).unwrap();
        a.handle(frame(FrameType::SnapshotReady, vec![])).unwrap();
        a.handle(frame(FrameType::SnapshotEnd, vec![])).unwrap();
        assert_eq!(a.resume().unwrap().incarnation, "second");
    }

    #[test]
    fn gap_discards_and_errors_classify() {
        let mut a: Attachment = Attachment::new();
        let gap = Gap { from: 1, to: 9 };
        assert_eq!(
            a.handle(json(FrameType::Gap, &gap)).unwrap(),
            vec![Action::Gap(gap)]
        );
        assert!(!a.ended());
        let soft = ErrorMessage {
            id: None,
            code: proto::ErrorCode::ReadOnly,
            message: String::new(),
        };
        assert_eq!(
            a.handle(json(FrameType::Error, &soft)).unwrap(),
            vec![Action::Error(soft.clone())]
        );
        assert!(!a.ended());
        let hard = ErrorMessage {
            id: None,
            code: proto::ErrorCode::TooSlow,
            message: String::new(),
        };
        a.handle(json(FrameType::Error, &hard)).unwrap();
        assert!(a.ended());
    }
}
