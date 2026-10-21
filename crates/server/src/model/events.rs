//! `Retained` against `Clankermux.Events`: events of random frame sizes,
//! and `subscribe {after}` near the oldest and the latest in the server's
//! incarnation.

use proptest::prelude::*;
use serde_json::{Value, json};

use crate::retained::{Replay, Retained};

#[derive(Debug, Clone)]
enum Op {
    Event(usize),
    /// A `subscribe` with `after` this far below the latest plus one.
    Subscribe(u64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (1usize..9).prop_map(Op::Event),
        2 => (0u64..7).prop_map(Op::Subscribe),
    ]
}

/// The model's case and what each operation observes.
fn run(max_events: u64, max_bytes: u64, ops: &[Op]) -> (Value, Vec<Value>) {
    let mut retained = Retained::new(max_events, max_bytes);
    let mut case = Vec::new();
    let mut seen = Vec::new();
    for op in ops {
        let (sent, observed) = match *op {
            Op::Event(size) => {
                retained.next_seq();
                retained.retain(vec![0; size]);
                (json!({"event": {"size": size}}), json!(retained.cursor()))
            }
            Op::Subscribe(back) => {
                let (_, latest) = retained.cursor();
                let after = (latest + 1).saturating_sub(back);
                let replayed = match retained.replay(after) {
                    Replay::Ahead => json!("ahead"),
                    Replay::Gap => Value::Null,
                    Replay::Events(events) => {
                        let events = events.into_iter().map(|(seq, frame)| (frame.len(), seq));
                        json!(events.collect::<Vec<_>>())
                    }
                };
                (json!({"subscribe": {"after": after}}), replayed)
            }
        };
        case.push(sent);
        seen.push(observed);
    }
    let limits = json!({"events": max_events, "bytes": max_bytes, "ops": case});
    (json!({"events": limits}), seen)
}

proptest! {
    #[test]
    #[ignore = "needs the Lean model: just lean"]
    fn retained_events_follow_the_model(
        max_events in 1u64..6,
        max_bytes in 1u64..24,
        ops in prop::collection::vec(op(), 1..32),
    ) {
        let (case, seen) = run(max_events, max_bytes, &ops);
        prop_assert_eq!(seen, super::observe(&case));
    }
}
