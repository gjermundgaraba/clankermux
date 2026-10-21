/-!
# Program status records and attention

PROTOCOL.md, "Program status", with the record rules of the engine's
program-status callback (`terminal.h`): one record per id, a report
replaces its record, a `clear` removes the record and every record
beneath it, `working` and `blocked` records go at a prompt, at the end of
the stream and on a load, at most `max` records with the least recently
updated evicted, and `attention` the most urgent record that wants the
user. A record keeps only what the attention reads.
-/

namespace Clankermux.Attention

inductive State where
  | idle | working | done | blocked | error
  deriving DecidableEq, Repr

/-- A record id as its `/`-separated segments; the root is `[]`. -/
abbrev Id := List String

structure Record where
  id : Id
  state : State
  app : Option String
  /-- On `done` and `error` records only. -/
  seen : Option Bool
  /-- When it was last reported: a later report has a higher tick. -/
  tick : Nat
  deriving DecidableEq, Repr

structure Records where
  /-- Oldest report first. -/
  recs : List Record
  /-- The tick of the latest report. -/
  tick : Nat
  deriving Repr

def empty : Records := ⟨[], 0⟩

/-- `done` and `error`, the states that carry `seen`. -/
def seeable : State → Bool
  | .done | .error => true
  | _ => false

/-- `working` and `blocked`, the states the program is still in. -/
def transient : State → Bool
  | .working | .blocked => true
  | _ => false

/-- A reported record: unseen when its state carries `seen`. -/
def fresh (id : Id) (state : State) (app : Option String) (tick : Nat) : Record :=
  { id, state, app, seen := if seeable state then some false else none, tick }

/-- A report: the record with this id replaced; a new id beyond `max`
records evicts the least recently updated. -/
def report (rs : Records) (id : Id) (state : State) (app : Option String) (max : Nat) :
    Records :=
  let recs := rs.recs.filter (·.id != id) ++ [fresh id state app (rs.tick + 1)]
  let new := rs.recs.all (·.id != id)
  { recs := if new && max < recs.length then recs.tail else recs, tick := rs.tick + 1 }

/-- A `clear` report: the record and every record beneath it; the root's
clears them all. -/
def clear (rs : Records) (id : Id) : Records :=
  { rs with recs := rs.recs.filter (!id.isPrefixOf ·.id) }

/-- A prompt start or the end of the stream. -/
def dropTransient (rs : Records) : Records :=
  { rs with recs := rs.recs.filter (!transient ·.state) }

/-- A park and a load: the transient records go, the rest keep their
order and `seen`. -/
def park (rs : Records) : Records := dropTransient rs

/-- Whether `ack` names the record: all of them when it names none. -/
def named (ids : Option (List Id)) (r : Record) : Bool :=
  match ids with
  | none => true
  | some ids => ids.contains r.id

def ackOne (ids : Option (List Id)) (r : Record) : Record :=
  if r.seen = some false ∧ named ids r then { r with seen := some true } else r

def ack (rs : Records) (ids : Option (List Id)) : Records :=
  { rs with recs := rs.recs.map (ackOne ids) }

/-- The ancestors of an id, nearest first, the root last. -/
def ancestors (id : Id) : List Id := (List.range id.length).reverse.map id.take

/-- The record's `app`, else that of the nearest ancestor present with one. -/
def appOf (rs : Records) (r : Record) : Option String :=
  r.app <|> (ancestors r.id).findSome? fun a => (rs.recs.find? (·.id == a)).bind (·.app)

/-- How urgently a record wants the user, most urgent lowest: `blocked`,
an unseen `error`, an unseen `done`, `working`; `none` when it does not. -/
def urgency (r : Record) : Option Nat :=
  match r.state, r.seen with
  | .blocked, _ => some 0
  | .error, some true => none
  | .error, _ => some 1
  | .done, some true => none
  | .done, _ => some 2
  | .working, _ => some 3
  | .idle, _ => none

/-- The urgency as a rank, 4 for a record that does not want the user. -/
def rank (r : Record) : Nat := (urgency r).getD 4

/-- Whether `a` wins the attention over `b`: more urgent, then the
shallower id, then the later report. -/
def beats (a b : Record) : Bool :=
  rank a < rank b || (rank a == rank b && (a.id.length < b.id.length ||
    (a.id.length == b.id.length && b.tick < a.tick)))

def pick (best : Option Record) (r : Record) : Option Record :=
  match best with
  | none => some r
  | some b => if beats r b then some r else some b

/-- The record the attention shows, absent when none wants the user. -/
def attention (rs : Records) : Option Record :=
  (rs.recs.filter (urgency · |>.isSome)).foldl pick none

/-! ## What every operation keeps -/

/-- Records in report order with ticks up to the latest, one per id,
`seen` on `done` and `error` records only. -/
structure Inv (rs : Records) : Prop where
  sorted : rs.recs.Pairwise (·.tick < ·.tick)
  bounded : ∀ r ∈ rs.recs, r.tick ≤ rs.tick
  unique : (rs.recs.map (·.id)).Nodup
  seen : ∀ r ∈ rs.recs, r.seen.isSome = seeable r.state

theorem Inv.sublist {rs rs' : Records} (h : Inv rs) (s : rs'.recs.Sublist rs.recs)
    (ht : rs'.tick = rs.tick) : Inv rs' where
  sorted := h.sorted.sublist s
  bounded r hr := ht ▸ h.bounded r (s.subset hr)
  unique := h.unique.sublist (s.map _)
  seen r hr := h.seen r (s.subset hr)

theorem inv_empty : Inv empty := by
  constructor <;> simp [empty]

theorem inv_report (rs : Records) (id : Id) (st : State) (app : Option String) (max : Nat)
    (h : Inv rs) : Inv (report rs id st app max) := by
  let r := fresh id st app (rs.tick + 1)
  have full : Inv ⟨rs.recs.filter (·.id != id) ++ [r], rs.tick + 1⟩ := by
    constructor
    · rw [List.pairwise_append]
      refine ⟨h.sorted.sublist List.filter_sublist, by simp, ?_⟩
      intro a ha b hb
      simp only [List.mem_singleton] at hb
      subst hb
      have := h.bounded a (List.mem_filter.mp ha).1
      simp only [r, fresh]
      omega
    · intro a ha
      simp only [List.mem_append, List.mem_singleton] at ha
      rcases ha with ha | ha
      · exact Nat.le_succ_of_le (h.bounded a (List.mem_filter.mp ha).1)
      · subst ha
        simp [r, fresh]
    · rw [List.map_append, List.nodup_append]
      refine ⟨h.unique.sublist (List.filter_sublist.map _), by simp, ?_⟩
      intro a ha b hb
      simp only [List.mem_map, List.mem_filter, bne_iff_ne, ne_eq] at ha
      simp only [List.mem_map, List.mem_singleton] at hb
      obtain ⟨x, ⟨-, hx⟩, rfl⟩ := ha
      obtain ⟨y, rfl, rfl⟩ := hb
      exact hx
    · intro a ha
      simp only [List.mem_append, List.mem_singleton] at ha
      rcases ha with ha | ha
      · exact h.seen a (List.mem_filter.mp ha).1
      · subst ha
        cases hs : seeable st <;> simp [r, fresh, hs]
  refine full.sublist ?_ rfl
  simp only [report]
  split
  · exact List.tail_sublist _
  · exact List.Sublist.refl _

theorem inv_clear (rs : Records) (id : Id) (h : Inv rs) : Inv (clear rs id) :=
  h.sublist List.filter_sublist rfl

theorem inv_dropTransient (rs : Records) (h : Inv rs) : Inv (dropTransient rs) :=
  h.sublist List.filter_sublist rfl

theorem inv_park (rs : Records) (h : Inv rs) : Inv (park rs) := inv_dropTransient rs h

theorem ackOne_id (ids : Option (List Id)) (r : Record) : (ackOne ids r).id = r.id := by
  unfold ackOne; split <;> rfl

theorem ackOne_tick (ids : Option (List Id)) (r : Record) : (ackOne ids r).tick = r.tick := by
  unfold ackOne; split <;> rfl

theorem inv_ack (rs : Records) (ids : Option (List Id)) (h : Inv rs) : Inv (ack rs ids) where
  sorted := by
    simp only [ack, List.pairwise_map, ackOne_tick]
    exact h.sorted
  bounded r hr := by
    simp only [ack, List.mem_map] at hr
    obtain ⟨x, hx, rfl⟩ := hr
    rw [ackOne_tick]
    exact h.bounded x hx
  unique := by
    simp only [ack, List.map_map]
    have : ((·.id) ∘ ackOne ids) = (·.id) := funext (ackOne_id ids)
    rw [this]
    exact h.unique
  seen r hr := by
    simp only [ack, List.mem_map] at hr
    obtain ⟨x, hx, rfl⟩ := hr
    have := h.seen x hx
    unfold ackOne
    split
    · next hs => simp_all
    · exact this

/-- At most `max` records. -/
theorem report_length (rs : Records) (id : Id) (st : State) (app : Option String) (max : Nat)
    (h : rs.recs.length ≤ max) : (report rs id st app max).recs.length ≤ max := by
  have := List.length_filter_le (fun x => x.id != id) rs.recs
  simp only [report]
  split
  · simp only [List.length_tail, List.length_append, List.length_singleton]
    omega
  · next hn =>
    simp only [Bool.and_eq_true, decide_eq_true_eq, not_and, Nat.not_lt] at hn
    by_cases hnew : (rs.recs.all fun x => x.id != id) = true
    · exact hn hnew
    · obtain ⟨x, hx, rfl⟩ : ∃ x ∈ rs.recs, x.id = id := by
        simp only [List.all_eq_true, bne_iff_ne, ne_eq] at hnew
        exact Classical.byContradiction fun hc => hnew fun x hx he => hc ⟨x, hx, he⟩
      have : (rs.recs.filter (·.id != x.id)).length < rs.recs.length :=
        List.length_filter_lt_length_iff_exists.mpr ⟨x, hx, by simp⟩
      simp only [List.length_append, List.length_singleton]
      omega

/-! ## Who wants the user -/

theorem urgency_idle (r : Record) (h : r.state = .idle) : urgency r = none := by
  unfold urgency; rw [h]

/-- A record the user has seen never counts. -/
theorem urgency_seen (rs : Records) (h : Inv rs) (r : Record) (hr : r ∈ rs.recs)
    (hs : r.seen = some true) : urgency r = none := by
  have := h.seen r hr
  unfold urgency
  rw [hs] at this ⊢
  cases hst : r.state <;> simp_all [seeable]

/-- `ack` never makes a record more urgent. -/
theorem ack_not_more_urgent (ids : Option (List Id)) (r : Record) :
    rank r ≤ rank (ackOne ids r) := by
  unfold ackOne
  split
  · next h =>
    obtain ⟨hs, -⟩ := h
    unfold rank urgency
    rw [hs]
    cases r.state <;> simp
  · exact Nat.le_refl _

theorem beats_trans {a b c : Record} (hab : beats a b) (hbc : beats b c) : beats a c := by
  simp only [beats, Bool.or_eq_true, decide_eq_true_eq, Bool.and_eq_true, beq_iff_eq] at *
  omega

theorem beats_irrefl (a : Record) : ¬ beats a a := by
  simp [beats]

theorem beats_asymm {a b : Record} (h : beats a b) : ¬ beats b a :=
  fun h' => beats_irrefl a (beats_trans h h')

theorem beats_total {a b : Record} (h : a.tick ≠ b.tick) : beats a b ∨ beats b a := by
  simp only [beats, Bool.or_eq_true, decide_eq_true_eq, Bool.and_eq_true, beq_iff_eq]
  omega

/-- What `pick` holds after the records `seen`: none for none, else one
of them that beats every other. -/
def Wins : Option Record → List Record → Prop
  | none, seen => seen = []
  | some w, seen => w ∈ seen ∧ ∀ c ∈ seen, c ≠ w → beats w c

theorem wins_pick (acc : Option Record) (seen : List Record) (x : Record)
    (hl : (seen ++ [x]).Pairwise (·.tick < ·.tick)) (h : Wins acc seen) :
    Wins (pick acc x) (seen ++ [x]) := by
  rw [List.pairwise_append] at hl
  have hx : ∀ c ∈ seen, c.tick < x.tick := fun c hc => hl.2.2 c hc x (by simp)
  cases acc with
  | none =>
    simp only [Wins] at h
    subst h
    simp [pick, Wins]
  | some w =>
    obtain ⟨hw, hwin⟩ := h
    simp only [pick]
    split
    · next hxw =>
      refine ⟨by simp, fun c hc hne => ?_⟩
      simp only [List.mem_append, List.mem_singleton] at hc
      rcases hc with hc | rfl
      · by_cases hcw : c = w
        · subst hcw; exact hxw
        · exact beats_trans hxw (hwin c hc hcw)
      · exact absurd rfl hne
    · next hxw =>
      refine ⟨by simp [hw], fun c hc hne => ?_⟩
      simp only [List.mem_append, List.mem_singleton] at hc
      rcases hc with hc | rfl
      · exact hwin c hc hne
      · have := hx w hw
        rcases beats_total (a := w) (b := c) (by omega) with h | h
        · exact h
        · exact absurd h hxw

theorem wins_foldl (l seen : List Record) (acc : Option Record)
    (hl : (seen ++ l).Pairwise (·.tick < ·.tick)) (h : Wins acc seen) :
    Wins (l.foldl pick acc) (seen ++ l) := by
  induction l generalizing seen acc with
  | nil => simpa using h
  | cons x l ih =>
    rw [List.foldl_cons, ← List.singleton_append, ← List.append_assoc]
    rw [← List.singleton_append, ← List.append_assoc] at hl
    exact ih _ _ hl (wins_pick acc seen x (hl.sublist (List.sublist_append_left _ _)) h)

theorem attention_wins (rs : Records) (h : Inv rs) :
    Wins (attention rs) (rs.recs.filter (urgency · |>.isSome)) := by
  have := wins_foldl (rs.recs.filter (urgency · |>.isSome)) [] none
    (by simpa using h.sorted.sublist List.filter_sublist) rfl
  simpa [attention] using this

theorem attention_none (rs : Records) (h : Inv rs) :
    attention rs = none ↔ ∀ r ∈ rs.recs, urgency r = none := by
  constructor
  · intro hn
    have := attention_wins rs h
    rw [hn, Wins, List.filter_eq_nil_iff] at this
    intro r hr
    simpa using this r hr
  · intro hn
    unfold attention
    rw [List.filter_eq_nil_iff.mpr (by simp_all)]
    rfl

/-- The attention is a record that wants the user and beats every other
that does: exactly one wins. -/
theorem attention_some (rs : Records) (h : Inv rs) (w : Record) (hw : attention rs = some w) :
    w ∈ rs.recs ∧ (urgency w).isSome ∧
      ∀ c ∈ rs.recs, (urgency c).isSome → c ≠ w → beats w c := by
  have := attention_wins rs h
  rw [hw] at this
  obtain ⟨hm, hwin⟩ := this
  rw [List.mem_filter] at hm
  exact ⟨hm.1, hm.2, fun c hc hu hne => hwin c (List.mem_filter.mpr ⟨hc, hu⟩) hne⟩

end Clankermux.Attention
