//! `Stream` against `Clankermux.Stream`: appends, markers, an attachment
//! served everything in small pieces or a gap, resumes, `seq_at` and
//! `read` near the retained start.

use proptest::prelude::*;
use serde_json::{Value, json};

use crate::stream::{Cursor, MarkerKind, Served, Stream};

#[derive(Debug, Clone)]
enum Op {
    Bytes(Vec<u8>),
    Marker,
    Serve,
    /// A resume `back` bytes before one past the end, with a marker
    /// `below` under the next `seq`.
    Resume {
        back: u64,
        below: u64,
    },
    SeqAt {
        back: u64,
    },
    Read {
        back: u64,
        max: usize,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => prop::collection::vec(any::<u8>(), 1..6).prop_map(Op::Bytes),
        2 => Just(Op::Marker),
        3 => Just(Op::Serve),
        1 => (0u64..16, 0u64..5).prop_map(|(back, below)| Op::Resume { back, below }),
        1 => (0u64..16).prop_map(|back| Op::SeqAt { back }),
        1 => (0u64..16, 0usize..6).prop_map(|(back, max)| Op::Read { back, max }),
    ]
}

/// The stream and one attachment's cursor: the model's case and what
/// each operation observes.
fn run(
    ring: usize,
    markers: usize,
    chunk: usize,
    budget: usize,
    ops: &[Op],
) -> (Value, Vec<Value>) {
    let mut s = Stream::new(ring, markers);
    let mut cursor = Cursor {
        offset: 0,
        marker: 0,
    };
    let bootstrap = |s: &Stream| Cursor {
        offset: s.offset(),
        marker: s.latest_seq(),
    };
    let mut case = Vec::new();
    let mut seen = Vec::new();
    for op in ops {
        let (sent, observed) = match op {
            Op::Bytes(bytes) => {
                s.append(bytes);
                (json!({"bytes": {"values": bytes}}), Value::Null)
            }
            Op::Marker => {
                s.add_marker(MarkerKind::Resized { cols: 1, rows: 1 });
                (json!("marker"), Value::Null)
            }
            Op::Serve => {
                let mut events = Vec::new();
                let observed = if s.retains(cursor) {
                    loop {
                        let served = s.serve(&mut cursor, chunk, budget);
                        if served.is_empty() {
                            break json!(events);
                        }
                        for item in served {
                            match item {
                                Served::Marker(m) => events
                                    .push(json!({"marker": {"seq": m.seq, "offset": m.offset}})),
                                Served::Output(offset, first, second) => {
                                    for (i, b) in first.iter().chain(second).enumerate() {
                                        let offset = offset + i as u64;
                                        events
                                            .push(json!({"byte": {"offset": offset, "value": b}}));
                                    }
                                }
                            }
                        }
                    }
                } else {
                    cursor = bootstrap(&s);
                    Value::Null
                };
                (json!("serve"), observed)
            }
            Op::Resume { back, below } => {
                let resume = Cursor {
                    offset: (s.offset() + 1).saturating_sub(*back),
                    marker: (s.latest_seq() + 1).saturating_sub(*below),
                };
                let resumes = s.retains(resume);
                cursor = if resumes { resume } else { bootstrap(&s) };
                let (offset, marker) = (resume.offset, resume.marker);
                (
                    json!({"resume": {"offset": offset, "marker": marker}}),
                    json!(resumes),
                )
            }
            Op::SeqAt { back } => {
                let offset = s.offset().saturating_sub(*back);
                let observed = (s.retained_start() <= offset).then(|| s.seq_at(offset));
                (json!({"seqAt": {"offset": offset}}), json!(observed))
            }
            Op::Read { back, max } => {
                let offset = (s.offset() + 1).saturating_sub(*back);
                let observed = s.read(offset, *max);
                (
                    json!({"read": {"offset": offset, "max": max}}),
                    json!(observed),
                )
            }
        };
        case.push(sent);
        seen.push(observed);
    }
    let case = json!({"stream": {"ring": ring, "markers": markers, "ops": case}});
    (case, seen)
}

proptest! {
    #[test]
    #[ignore = "needs the Lean model: just lean"]
    fn the_stream_follows_the_model(
        ring in 1usize..12,
        markers in 1usize..5,
        chunk in 1usize..5,
        budget in 1usize..7,
        ops in prop::collection::vec(op(), 1..40),
    ) {
        let (case, seen) = run(ring, markers, chunk, budget, &ops);
        prop_assert_eq!(seen, super::observe(&case));
    }
}
