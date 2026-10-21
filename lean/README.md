# Lean models of the protocol

Each module models part of PROTOCOL.md and proves what the protocol
promises of it. `Main.lean` runs the models as a process: the
conformance tests in `crates/server/src/model` drive the server's code
and a model with the same random operations and compare what each
observes. `just lean` builds the proofs (a `sorry` or any other warning
fails the build) and runs those tests.

| Module | Server | Proved |
| --- | --- | --- |
| `Attention` | `records.rs` | Records stay one per id, in report order, at most `max`, `seen` on `done` and `error` only; the attention is a record that wants the user and beats every other one that does; `idle` and seen records never count; `ack` never makes a record more urgent. |
| `Stream` | `stream.rs` | The retained window serves what the whole stream would whenever it retains the cursor, and does not retain it only when something the cursor needs is gone; bytes and markers are served once each, in order, each marker before any byte at or past its offset; serving where the last serve stopped continues it; a cursor `seqAt` places is either the one at its offset or not retained. |
| `Size` | `sizing.rs` | An owner is an open attachment, and only under `latest`; `smallest` is at most every recorded viewport and each dimension is one's; a report, a close and setting `smallest` apply it; `fixed` records and does not apply; `resize` applies under every policy with no owner; setting a policy clears the owner. |
| `Events` | `retained.rs` | The retained events are the longest run of the latest that fits both limits, and retaining one event at a time keeps exactly that; a replay holds every event above `after` once each and in order; `gap` exactly when one of them is gone or `after` is above the latest. |

The models use Lean's core library only; `Main.lean` also reads and
writes the cases as JSON with `Lean.Data.Json`. The toolchain is in
`lean-toolchain`; elan installs it.
