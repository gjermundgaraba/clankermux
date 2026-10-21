import Clankermux
import Lean.Data.Json

/-!
The models as a process, for the conformance tests in `crates/server`.
Each line on stdin is one case, a model and its operations as JSON;
each line on stdout is what the model observes after every operation,
a JSON array, or `error:` and why the case could not be read.
-/

open Lean (Json ToJson FromJson toJson fromJson?)
open Clankermux

/-- What a model observes after each of `ops`, `step` giving the next
state and what it observed. -/
def run (init : σ) (step : σ → α → σ × Json) (ops : List α) : Json :=
  toJson (ops.foldl (fun (st, out) op => let (st, seen) := step st op; (st, out.push seen))
    (init, (#[] : Array Json))).2

namespace Attention

open Clankermux.Attention

deriving instance ToJson, FromJson for State

/-- A record id as the server writes it, its parts joined by `/`. -/
def id (s : String) : Id := if s.isEmpty then [] else s.splitOn "/"

inductive Op where
  | report (id : String) (state : State) (app : Option String)
  | clear (id : String)
  /-- A prompt or the end of the stream. -/
  | drop
  /-- A park and a load. -/
  | park
  | ack (ids : Option (List String))
  deriving FromJson

structure Wanting where
  id : String
  state : State
  app : Option String
  deriving ToJson

structure Seen where
  id : String
  state : State
  seen : Option Bool
  deriving ToJson

/-- The attention, then the records in report order. -/
structure Observed where
  attention : Option Wanting
  records : List Seen
  deriving ToJson

def step (max : Nat) (rs : Records) (op : Op) : Records × Json :=
  let rs := match op with
    | .report i st app => report rs (id i) st app max
    | .clear i => clear rs (id i)
    | .drop => dropTransient rs
    | .park => park rs
    | .ack ids => ack rs (ids.map (·.map id))
  let observed : Observed := {
    attention := (attention rs).map fun r => ⟨"/".intercalate r.id, r.state, appOf rs r⟩
    records := rs.recs.map fun r => ⟨"/".intercalate r.id, r.state, r.seen⟩ }
  (rs, toJson observed)

end Attention

namespace Stream

open Clankermux.Stream

deriving instance ToJson for Event

structure Attached where
  s : Clankermux.Stream.Stream
  cursor : Cursor

inductive Op where
  | bytes (values : List Nat)
  | marker
  /-- Serves the attachment everything: the stream, or `null` for a
  gap; then a new bootstrap. -/
  | serve
  /-- `true` for a resume, `false` for a snapshot. -/
  | resume (offset marker : Nat)
  /-- `seqAt`, `null` outside the retained stream. -/
  | seqAt (offset : Nat)
  /-- `read`, `null` outside the retained stream. -/
  | read (offset max : Nat)
  deriving FromJson

/-- Whether an offset lies within the retained stream. -/
def within (s : Clankermux.Stream.Stream) (o : Nat) : Bool := start s ≤ o && o ≤ size s.log

def step (a : Attached) : Op → Attached × Json
  | .bytes values =>
    ({ a with s.log := values.foldl (fun log v => push log (.byte v)) a.s.log }, Json.null)
  | .marker => ({ a with s.log := push a.s.log .marker }, Json.null)
  | .serve =>
    let seen := if retains a.s a.cursor then some (served a.s.log a.cursor) else none
    ({ a with cursor := next a.s.log }, toJson seen)
  | .resume offset marker =>
    let c : Cursor := ⟨offset, marker⟩
    let resumes := retains a.s c
    ({ a with cursor := if resumes then c else next a.s.log }, toJson resumes)
  | .seqAt o => (a, toJson (if within a.s o then some (seqAt a.s o) else none))
  | .read o max =>
    let values := a.s.log.filterMap fun
      | .byte o' v => if o ≤ o' && o' < o + max then some v else none
      | .marker .. => none
    (a, toJson (if within a.s o then some values else none))

end Stream

namespace Size

open Clankermux.Size

deriving instance ToJson, FromJson for Policy
deriving instance ToJson for Sizing

inductive Op where
  /-- An `OPEN`, with the viewport of a writable one. -/
  | «open» (id : Nat) (viewport : Option Dims)
  /-- A `VIEWPORT` recorded. -/
  | viewport (id : Nat) (size : Dims)
  /-- A report from attachment `id` applied, open or not. -/
  | apply (id : Nat) (size : Dims)
  | close (id : Nat)
  | resize (size : Dims)
  | policy (policy : Policy)
  deriving FromJson

def step (st : Sizing) (op : Op) : Sizing × Json :=
  let st := match op with
    | .open id viewport => openAttachment st id viewport
    | .viewport id d => record st id d
    | .apply id d => apply st (.attachment id) d
    | .close id => close st id
    | .resize d => resize st d
    | .policy p => setPolicy st p
  (st, toJson st)

end Size

namespace Events

open Clankermux.Events

inductive Op where
  /-- An event whose frame has `size` bytes, observed as `oldest` and
  `latest`. -/
  | event (size : Nat)
  /-- `subscribe {after}`: `null` for a gap, else the replayed `size`
  and `seq` of each event. -/
  | subscribe (after : Nat)
  deriving FromJson

def step (l : Limits) (log : List Nat) : Op → List Nat × Json
  | .event size => let log := log ++ [size]; (log, toJson (oldest l log, latest log))
  | .subscribe after => (log, toJson (replay l log after))

end Events

/-- A model and its operations. -/
inductive Case where
  | attention (max : Nat) (ops : List Attention.Op)
  | stream (ring markers : Nat) (ops : List Stream.Op)
  | size (size : Size.Dims) (ops : List Size.Op)
  | events (events bytes : Nat) (ops : List Events.Op)
  deriving FromJson

def Case.run : Case → Json
  | .attention max ops => _root_.run Attention.empty (Attention.step max) ops
  | .stream ring markers ops => _root_.run ⟨⟨[], ring, markers⟩, ⟨0, 0⟩⟩ Stream.step ops
  | .size d ops => _root_.run (Size.new d) Size.step ops
  | .events n bytes ops => _root_.run [] (Events.step ⟨n, bytes⟩) ops

def answer (line : String) : String :=
  match Json.parse line >>= fromJson? with
  | .ok (c : Case) => c.run.compress
  | .error e => s!"error: {e}"

partial def loop (stdin : IO.FS.Stream) (stdout : IO.FS.Stream) : IO Unit := do
  let line ← stdin.getLine
  if line.isEmpty then return
  stdout.putStrLn (answer line)
  stdout.flush
  loop stdin stdout

def main : IO Unit := do
  loop (← IO.getStdin) (← IO.getStdout)
