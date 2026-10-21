//! `Sizing` against `Clankermux.Size`: opens with and without a viewport,
//! viewports recorded, reports applied (from attachments open or closed),
//! closes, `resize` and policies.

use proptest::prelude::*;
use proto::{Cells, SizePolicy};
use serde_json::{Value, json};

use crate::sizing::{Sizing, Source};

type Dims = (u16, u16);

#[derive(Debug, Clone)]
enum Op {
    Open(Option<Dims>),
    /// From the open attachment `pick` picks.
    Viewport(usize, Dims),
    /// From the attachment, open or closed, `pick` picks.
    Apply(usize, Dims),
    Close(usize),
    Resize(Dims),
    Policy(SizePolicy),
}

fn dims() -> impl Strategy<Value = Dims> {
    (1u16..5, 1u16..5)
}

fn op() -> impl Strategy<Value = Op> {
    let policy = prop::sample::select(vec![
        SizePolicy::Latest,
        SizePolicy::Smallest,
        SizePolicy::Fixed,
    ]);
    prop_oneof![
        3 => prop::option::of(dims()).prop_map(Op::Open),
        4 => (any::<usize>(), dims()).prop_map(|(pick, d)| Op::Viewport(pick, d)),
        3 => (any::<usize>(), dims()).prop_map(|(pick, d)| Op::Apply(pick, d)),
        2 => any::<usize>().prop_map(Op::Close),
        1 => dims().prop_map(Op::Resize),
        2 => policy.prop_map(Op::Policy),
    ]
}

/// As the model's `Sizing`.
fn observe(sizing: &Sizing, size: Dims) -> Value {
    let attached: Vec<_> = sizing
        .attached()
        .iter()
        .map(|(id, v)| json!([id, v.map(|v| (v.cols, v.rows))]))
        .collect();
    json!({
        "policy": sizing.policy(),
        "owner": sizing.owner(),
        "size": size,
        "attached": attached,
    })
}

fn cells((cols, rows): Dims) -> Cells {
    Cells { cols, rows }
}

/// The model's case and what each operation observes.
fn run(ops: &[Op]) -> (Value, Vec<Value>) {
    let mut sizing = Sizing::new(SizePolicy::Latest);
    let mut size = (80, 24);
    let mut next_id = 1;
    let mut case = Vec::new();
    let mut seen = Vec::new();
    for op in ops {
        let sent = match *op {
            Op::Open(viewport) => {
                let id = next_id;
                next_id += 1;
                sizing.open(id);
                if let Some(d) = viewport {
                    sizing.record(id, cells(d));
                    size = sizing.apply(Source::Attachment(id), d, size);
                }
                json!({"open": {"id": id, "viewport": viewport}})
            }
            Op::Viewport(pick, d) => {
                let attached = sizing.attached();
                let Some(&(id, _)) = attached.get(pick % attached.len().max(1)) else {
                    continue;
                };
                sizing.record(id, cells(d));
                json!({"viewport": {"id": id, "size": d}})
            }
            Op::Apply(pick, d) => {
                if next_id == 1 {
                    continue;
                }
                let id = 1 + pick as u64 % (next_id - 1);
                size = sizing.apply(Source::Attachment(id), d, size);
                json!({"apply": {"id": id, "size": d}})
            }
            Op::Close(pick) => {
                let attached = sizing.attached();
                if attached.is_empty() {
                    continue;
                }
                let (id, _) = attached[pick % attached.len()];
                if let Some(d) = sizing.close(id) {
                    size = d;
                }
                json!({"close": {"id": id}})
            }
            Op::Resize(d) => {
                size = sizing.apply(Source::Resize, d, size);
                json!({"resize": {"size": d}})
            }
            Op::Policy(policy) => {
                if let Some(d) = sizing.set_policy(policy) {
                    size = d;
                }
                json!({"policy": {"policy": policy}})
            }
        };
        case.push(sent);
        seen.push(observe(&sizing, size));
    }
    (json!({"size": {"size": (80, 24), "ops": case}}), seen)
}

proptest! {
    #[test]
    #[ignore = "needs the Lean model: just lean"]
    fn sizing_follows_the_model(ops in prop::collection::vec(op(), 1..32)) {
        let (case, seen) = run(&ops);
        prop_assert_eq!(seen, super::observe(&case));
    }
}
