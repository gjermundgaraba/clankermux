//! The attachment state machine: frames in, actions out. It keeps the
//! local engine current (bootstrapping from the snapshot, then feeding
//! the stream) and tracks the cursor for resuming.

use ghostty_vt::Terminal;
use ghostty_vt::snapshot::Decoder;
use proto::{
    AttachMode, Attached, Closed, ClosedReason, ErrorMessage, Exit, Exited, FrameType, Gap,
    Resized, Resume, Session, parse_output,
};

use crate::connection::{Error, Frame};
use crate::expect::Expect;

/// What a client does in response to frames.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// The screen is renderable from the local engine; paint it.
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
pub struct Cursor {
    pub offset: u64,
    pub marker: u64,
}

enum State {
    /// Before `ATTACHED`.
    Opening,
    Bootstrapping {
        decoder: Decoder<Expect>,
        /// Stream frames held back after a `RESIZED` until `SNAPSHOT_END`.
        held: Vec<Frame>,
    },
    Streaming(Terminal<Expect>),
    /// After the final marker, `CLOSED` or a fatal error.
    Ended(Option<Terminal<Expect>>),
}

pub struct Attachment {
    incarnation: String,
    continuation: usize,
    state: State,
    cursor: Cursor,
    pub session: Option<Session>,
    pub cols: u16,
    pub rows: u16,
    /// Whether output is passed to an outer terminal; see [`Expect`].
    passthrough: bool,
}

impl Attachment {
    pub fn new(incarnation: &str, continuation: usize) -> Self {
        Attachment {
            incarnation: incarnation.into(),
            continuation,
            state: State::Opening,
            cursor: Cursor {
                offset: 0,
                marker: 0,
            },
            session: None,
            cols: 0,
            rows: 0,
            passthrough: false,
        }
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
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

    pub fn bootstrapping(&self) -> bool {
        matches!(self.state, State::Bootstrapping { .. })
    }

    pub fn ended(&self) -> bool {
        matches!(self.state, State::Ended(_))
    }

    /// The local engine, once `SNAPSHOT_READY` has been reached.
    pub fn term(&mut self) -> Option<&mut Terminal<Expect>> {
        match &mut self.state {
            State::Opening => None,
            State::Bootstrapping { decoder, .. } => decoder.terminal(),
            State::Streaming(term) | State::Ended(Some(term)) => Some(term),
            State::Ended(None) => None,
        }
    }

    pub fn set_passthrough(&mut self, passthrough: bool) {
        self.passthrough = passthrough;
        if let Some(term) = self.term() {
            term.handler_mut().passthrough = passthrough;
        }
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
                self.cursor = Cursor {
                    offset: attached.offset,
                    marker: attached.marker,
                };
                self.cols = attached.cols;
                self.rows = attached.rows;
                self.session = Some(attached.session);
                match attached.mode {
                    AttachMode::Snapshot => {
                        let mut decoder =
                            Decoder::with_handler(Expect::new(self.passthrough)).map_err(engine)?;
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
                let State::Bootstrapping { decoder, held } =
                    std::mem::replace(&mut self.state, State::Opening)
                else {
                    return Err(Error::Protocol("SNAPSHOT_END outside a bootstrap".into()));
                };
                let mut decoder = decoder;
                while decoder.step_available() && decoder.next_page().map_err(engine)? {}
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
                    && (!held.is_empty() || kind == FrameType::Resized)
                {
                    held.push(frame);
                    return Ok(());
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
                    return Err(Error::Protocol(format!(
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
                self.cols = resized.cols;
                self.rows = resized.rows;
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
            return Err(Error::Protocol(format!(
                "marker {seq} at {offset}, expected {} at {}",
                self.cursor.marker + 1,
                self.cursor.offset
            )));
        }
        self.cursor.marker = seq;
        Ok(())
    }

    fn end(&mut self) {
        let term = match std::mem::replace(&mut self.state, State::Opening) {
            State::Streaming(term) | State::Ended(Some(term)) => Some(term),
            State::Bootstrapping { decoder, .. } => decoder.into_terminal(),
            State::Opening | State::Ended(None) => None,
        };
        self.state = State::Ended(term);
    }
}

fn engine(err: ghostty_vt::Error) -> Error {
    Error::Protocol(format!("engine: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostty_vt::snapshot;
    use proto::{Header, Status};

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

    fn text(term: &Terminal<Expect>) -> String {
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

    #[test]
    fn bootstraps_holds_after_resize_and_resumes() {
        let mut source = Terminal::new(10, 3).unwrap();
        source.vt_write(b"one\r\ntwo\r\n");
        let bytes = snapshot::encode(&source).unwrap();
        let ready = snapshot::ready_end(&bytes).unwrap();

        let mut a = Attachment::new("inc", 1 << 20);
        let attached = Attached {
            mode: AttachMode::Snapshot,
            offset: 100,
            marker: 2,
            cols: 10,
            rows: 3,
            session: session(10, 3),
        };
        assert_eq!(
            a.handle(json(FrameType::Attached, &attached)).unwrap(),
            vec![]
        );
        assert!(a.bootstrapping());
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
    fn gap_discards_and_errors_classify() {
        let mut a = Attachment::new("inc", 1 << 20);
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
