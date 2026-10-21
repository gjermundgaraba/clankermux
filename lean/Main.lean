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

/-- A model and its operations. -/
inductive Case where
  | attention (max : Nat) (ops : List Attention.Op)
  deriving FromJson

def Case.run : Case → Json
  | .attention max ops => _root_.run Attention.empty (Attention.step max) ops

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
