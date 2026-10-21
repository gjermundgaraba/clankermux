//! `Records` against `Clankermux.Attention`: reports, clears, prompts,
//! parks and acks, observed as the attention and the records in report
//! order.

use ghostty_vt::{ProgramStatus, ProgramStatusKind, ProgramStatusState};
use proptest::prelude::*;
use serde_json::{Value, json};

use crate::records::{Records, state_name};

#[derive(Debug, Clone)]
enum Op {
    Report(String, ProgramStatusState, Option<String>),
    Clear(String),
    /// A prompt start or the end of the stream.
    Drop,
    Park,
    Ack(Option<Vec<String>>),
}

fn id() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["", "a", "b", "a/b", "a/c", "b/a", "a/b/c"]).prop_map(String::from)
}

fn op() -> impl Strategy<Value = Op> {
    use ProgramStatusState::*;
    let state = prop::sample::select(vec![Idle, Working, Done, Blocked, Error]);
    let app = prop::option::of(prop::sample::select(vec!["x", "y"]).prop_map(String::from));
    prop_oneof![
        6 => (id(), state, app).prop_map(|(i, s, a)| Op::Report(i, s, a)),
        1 => id().prop_map(Op::Clear),
        1 => Just(Op::Drop),
        1 => Just(Op::Park),
        2 => prop::option::of(prop::collection::vec(id(), 0..3)).prop_map(Op::Ack),
    ]
}

fn status(state: ProgramStatusState, id: &str, app: Option<&str>) -> ProgramStatus {
    ProgramStatus {
        state,
        kind: ProgramStatusKind::None,
        progress: -1,
        id: id.into(),
        app: app.unwrap_or_default().into(),
        title: String::new(),
        message: String::new(),
    }
}

fn case(max: usize, ops: &[Op]) -> Value {
    let ops: Vec<_> = ops
        .iter()
        .map(|op| match op {
            Op::Report(i, s, a) => {
                json!({"report": {"id": i, "state": state_name(*s), "app": a}})
            }
            Op::Clear(i) => json!({"clear": {"id": i}}),
            Op::Drop => json!("drop"),
            Op::Park => json!("park"),
            Op::Ack(ids) => json!({"ack": {"ids": ids}}),
        })
        .collect();
    json!({"attention": {"max": max, "ops": ops}})
}

/// As the model's `Observed`: the attention, then the records in report
/// order.
fn observe(r: &Records) -> Value {
    let attention = r
        .attention()
        .map(|a| json!({"id": a.record, "state": a.state, "app": a.app}));
    let records: Vec<_> = r
        .all()
        .into_iter()
        .map(|r| json!({"id": r.id, "state": r.state, "seen": r.seen}))
        .collect();
    json!({"attention": attention, "records": records})
}

fn run(max: usize, ops: &[Op]) -> Vec<Value> {
    let mut r = Records::default();
    let mut out = Vec::new();
    for op in ops {
        match op {
            Op::Report(i, s, a) => {
                r.apply(status(*s, i, a.as_deref()), max, "t");
            }
            Op::Clear(i) => {
                r.apply(status(ProgramStatusState::Clear, i, None), max, "t");
            }
            Op::Drop => {
                r.drop_transient();
            }
            Op::Park => r = Records::load(r.all()).0,
            Op::Ack(ids) => {
                r.ack(ids.as_deref());
            }
        }
        out.push(observe(&r));
    }
    out
}

proptest! {
    #[test]
    #[ignore = "needs the Lean model: just lean"]
    fn records_follow_the_model(max in 1usize..5, ops in prop::collection::vec(op(), 1..24)) {
        prop_assert_eq!(run(max, &ops), super::observe(&case(max, &ops)));
    }
}
