/-!
# Numbered events and their replay

PROTOCOL.md, "Events" and `subscribe`: the events of an incarnation are
numbered from 1, the latest `limits.events` are retained, fewer when
their frames would exceed a byte limit, and `subscribe {after}` replays
the retained events above `after`, or answers `gap` when some are gone
or `after` is above the latest.
An event is its frame's size; the incarnation is left out.
-/

namespace Clankermux.Events

structure Limits where
  events : Nat
  bytes : Nat

/-- Whether events of these sizes fit the limits. -/
def fits (l : Limits) (sizes : List Nat) : Bool :=
  sizes.length ≤ l.events && sizes.sum ≤ l.bytes

/-- The retained events: the longest run of the latest that fits. -/
def retained (l : Limits) : List Nat → List Nat
  | [] => []
  | e :: rest => if fits l (e :: rest) then e :: rest else retained l rest

/-- The `seq` of the latest event, 0 before the first: every event has
the next. -/
def latest (log : List Nat) : Nat := log.length

/-- The `seq` of the oldest retained event, `latest + 1` when none is. -/
def oldest (l : Limits) (log : List Nat) : Nat := log.length - (retained l log).length + 1

/-- The retained events with their `seq`s. -/
def numbered (l : Limits) (log : List Nat) : List (Nat × Nat) :=
  (retained l log).zipIdx (oldest l log)

/-- `subscribe {after}` in the server's incarnation: the retained events
above `after`, in order, or `none`, a gap. -/
def replay (l : Limits) (log : List Nat) (after : Nat) : Option (List (Nat × Nat)) :=
  if latest log < after || after + 1 < oldest l log then none
  else some ((numbered l log).filter (after < ·.2))

/-- The server's way: retain the new event, then drop the oldest while
the retained ones do not fit. -/
def push (l : Limits) (kept : List Nat) (e : Nat) : List Nat := retained l (kept ++ [e])

/-! ## What is retained -/

theorem retained_suffix (l : Limits) (log : List Nat) : retained l log <:+ log := by
  induction log with
  | nil => exact List.suffix_refl _
  | cons e rest ih =>
    unfold retained
    split
    · exact List.suffix_refl _
    · exact ih.trans (List.suffix_cons _ _)

theorem retained_fits (l : Limits) (log : List Nat) : fits l (retained l log) := by
  induction log with
  | nil => simp [retained, fits]
  | cons e rest ih =>
    unfold retained
    split
    · assumption
    · exact ih

/-- No longer run of the latest events fits. -/
theorem retained_longest (l : Limits) (log s : List Nat) (hs : s <:+ log) (hf : fits l s) :
    s.length ≤ (retained l log).length := by
  induction log generalizing s with
  | nil => simp_all
  | cons e rest ih =>
    unfold retained
    split
    · exact hs.length_le
    · next hn =>
      rcases List.suffix_cons_iff.mp hs with h | h
      · subst h; exact absurd hf hn
      · exact ih s h hf

theorem fits_of_append (l : Limits) (xs : List Nat) (e : Nat) (h : fits l (xs ++ [e])) :
    fits l xs := by
  simp only [fits, Bool.and_eq_true, decide_eq_true_eq, List.length_append, List.sum_append,
    List.length_singleton, List.sum_singleton] at h ⊢
  omega

theorem retained_cons (l : Limits) (e : Nat) (rest : List Nat) :
    retained l (e :: rest) = if fits l (e :: rest) then e :: rest else retained l rest := rfl

/-- Retaining as the server does, one event at a time, retains what the
protocol says. -/
theorem push_eq (l : Limits) (log : List Nat) (e : Nat) :
    push l (retained l log) e = retained l (log ++ [e]) := by
  unfold push
  induction log with
  | nil => rfl
  | cons x rest ih =>
    rw [List.cons_append, retained_cons]
    by_cases hf : fits l (x :: (rest ++ [e])) = true
    · have hr : fits l (x :: rest) = true := fits_of_append l (x :: rest) e hf
      simp only [hf, hr, ↓reduceIte, List.cons_append, retained_cons]
    · by_cases hr : fits l (x :: rest) = true
      · simp only [hr, ↓reduceIte, List.cons_append]
      · simp only [hr, ↓reduceIte, Bool.false_eq_true, ih, retained_cons, hf]

/-! ## The `seq`s -/

theorem oldest_le (l : Limits) (log : List Nat) : oldest l log ≤ latest log + 1 := by
  unfold oldest latest; omega

/-- The retained events are numbered from `oldest` to `latest`, one each. -/
theorem numbered_seqs (l : Limits) (log : List Nat) :
    (numbered l log).map (·.2) = List.range' (oldest l log) (retained l log).length := by
  simp [numbered, List.zipIdx_eq_zip_range', List.map_snd_zip]

theorem retained_length_le (l : Limits) (log : List Nat) :
    (retained l log).length ≤ log.length := (retained_suffix l log).length_le

/-- A replay holds every event above `after`, once each and in order. -/
theorem replay_events (l : Limits) (log : List Nat) (after : Nat) (seqs : List (Nat × Nat))
    (h : replay l log after = some seqs) :
    seqs.map (·.2) = List.range' (after + 1) (latest log - after) := by
  unfold replay at h
  split at h
  · simp at h
  · next hc =>
    simp only [Option.some.injEq] at h
    subst h
    simp only [Bool.or_eq_true, decide_eq_true_eq, not_or] at hc
    obtain ⟨ha, hg⟩ := hc
    have hm : ((numbered l log).filter fun x => decide (after < x.2)).map (·.2) =
        ((numbered l log).map (·.2)).filter fun x => decide (after < x) := by
      rw [List.filter_map]; rfl
    rw [hm, numbered_seqs]
    have hl := retained_length_le l log
    simp only [latest, oldest, Nat.not_lt] at ha hg ⊢
    generalize (retained l log).length = k at hl hg ⊢
    generalize log.length = n at hl hg ha ⊢
    have hsplit := @List.range'_append_1 (n - k + 1) (after - (n - k)) (n - after)
    rw [show n - k + 1 + (after - (n - k)) = after + 1 by omega,
      show after - (n - k) + (n - after) = k by omega] at hsplit
    rw [← hsplit, List.filter_append, List.filter_eq_nil_iff.mpr, List.filter_eq_self.mpr,
      List.nil_append]
    · intro x hx; simp [List.mem_range'_1] at hx ⊢; omega
    · intro x hx; simp [List.mem_range'_1] at hx ⊢; omega

/-- A gap exactly when `after` is above the latest event or an event
above it is no longer retained. -/
theorem replay_gap (l : Limits) (log : List Nat) (after : Nat) :
    replay l log after = none ↔ latest log < after ∨
      ∃ seq, after < seq ∧ seq ≤ latest log ∧ seq ∉ (numbered l log).map (·.2) := by
  rw [numbered_seqs]
  have hl := retained_length_le l log
  unfold replay
  split
  · next hc =>
    simp only [true_iff]
    simp only [Bool.or_eq_true, decide_eq_true_eq] at hc
    simp only [oldest, latest] at hc ⊢
    rcases hc with ha | hg
    · exact .inl ha
    · exact .inr ⟨after + 1, by omega, by omega, by simp [List.mem_range'_1]; omega⟩
  · next hc =>
    simp only [reduceCtorEq, false_iff, not_or, not_exists, not_and, Classical.not_not]
    simp only [Bool.or_eq_true, decide_eq_true_eq, not_or] at hc
    simp only [oldest, latest] at hc ⊢
    refine ⟨by omega, ?_⟩
    intro seq h1 h2
    simp [List.mem_range'_1]
    omega

end Clankermux.Events
