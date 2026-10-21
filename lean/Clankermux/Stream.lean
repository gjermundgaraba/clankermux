/-!
# The stream, cursors and gaps

PROTOCOL.md, "Stream, offsets and markers", "Slow attachments" and
"Bootstrap, mode `resume`". The model keeps the whole stream, every byte
and marker in the order they happened; what the server retains is a
window on it: the last `ring` bytes, and the last `maxMarkers` markers
whose offsets lie within them. Serving a cursor is defined on the whole
stream, and the proofs show the window serves the same whenever it
retains the cursor, that it does not retain the cursor only when
something the cursor needs is gone, and that serving neither skips nor
repeats a byte or a marker.
-/

namespace Clankermux.Stream

inductive Event where
  | byte (offset : Nat) (value : Nat)
  | marker (seq : Nat) (offset : Nat)
  deriving DecidableEq, Repr

/-- The bytes' offsets, in stream order. -/
def offsets (log : List Event) : List Nat :=
  log.filterMap fun | .byte o _ => some o | .marker .. => none

/-- The markers as `(seq, offset)`, in stream order. -/
def markers (log : List Event) : List (Nat × Nat) :=
  log.filterMap fun | .marker q o => some (q, o) | .byte .. => none

/-- The offset of the next byte. -/
def size (log : List Event) : Nat := (offsets log).length

/-- The `seq` of the latest marker, 0 before the first. -/
def latest (log : List Event) : Nat := (markers log).length

inductive Op where
  | byte (value : Nat)
  | marker

/-- One byte, at the next offset, or one marker, with the next `seq` and
the offset of the next byte. -/
def push (log : List Event) : Op → List Event
  | .byte v => log ++ [.byte (size log) v]
  | .marker => log ++ [.marker (latest log + 1) (size log)]

/-- The streams a session can have. -/
inductive Wf : List Event → Prop where
  | nil : Wf []
  | step {log : List Event} (op : Op) : Wf log → Wf (push log op)

structure Stream where
  /-- Every byte and marker, in the order they happened. -/
  log : List Event
  /-- `limits.ring` -/
  ring : Nat
  /-- `limits.markers` -/
  maxMarkers : Nat

/-- The offset of the first retained byte. -/
def start (s : Stream) : Nat := size s.log - s.ring

/-- The last `maxMarkers` markers whose offsets lie within the retained
bytes. -/
def retainedMarkers (s : Stream) : List (Nat × Nat) :=
  let within := (markers s.log).filter (start s ≤ ·.2)
  within.drop (within.length - s.maxMarkers)

def retained (s : Stream) : Event → Bool
  | .byte o _ => start s ≤ o
  | .marker q o => (retainedMarkers s).contains (q, o)

structure Cursor where
  /-- The next byte to receive. -/
  offset : Nat
  /-- The latest marker already reflected. -/
  marker : Nat
  deriving DecidableEq, Repr

/-- Where a cursor is once it has everything. -/
def next (log : List Event) : Cursor := ⟨size log, latest log⟩

def after (c : Cursor) : Event → Bool
  | .byte o _ => c.offset ≤ o
  | .marker q _ => c.marker < q

/-- The stream from a cursor: the bytes from its offset and the markers
above its marker, as they happened, so each marker before any byte at or
past its offset. -/
def served (log : List Event) (c : Cursor) : List Event := log.filter (after c)

/-- What the retained window has of the stream from a cursor. -/
def servedRetained (s : Stream) (c : Cursor) : List Event :=
  (s.log.filter (retained s)).filter (after c)

/-- Whether the server can serve a cursor: its offset lies within the
retained stream, its marker is at most the latest, and every marker
above it is retained. Else a slow attachment gets `GAP` and a resume a
snapshot. -/
def retains (s : Stream) (c : Cursor) : Bool :=
  start s ≤ c.offset && c.offset ≤ size s.log && c.marker ≤ latest s.log &&
    (markers s.log).all fun m => !(c.marker < m.1) || (retainedMarkers s).contains m

/-- The latest marker at or before an offset, as a cursor there reflects
it. -/
def reflected (log : List Event) (o : Nat) : Nat := ((markers log).filter (·.2 ≤ o)).length

/-- `Waited.at`'s marker: the reflected one while every marker after the
offset is retained, else 0, which claims none. -/
def seqAt (s : Stream) (o : Nat) : Nat :=
  if (markers s.log).all fun m => !(o < m.2) || (retainedMarkers s).contains m then
    reflected s.log o
  else 0

/-! ## Streams as they happen -/

theorem offsets_append (a b : List Event) : offsets (a ++ b) = offsets a ++ offsets b := by
  simp [offsets, List.filterMap_append]

theorem markers_append (a b : List Event) : markers (a ++ b) = markers a ++ markers b := by
  simp [markers, List.filterMap_append]

theorem offsets_push (log : List Event) (op : Op) :
    offsets (push log op) = offsets log ++ match op with
      | .byte _ => [size log]
      | .marker => [] := by
  cases op <;> simp [push, offsets]

theorem markers_push (log : List Event) (op : Op) :
    markers (push log op) = markers log ++ match op with
      | .byte _ => []
      | .marker => [(latest log + 1, size log)] := by
  cases op <;> simp [push, markers]

/-- The bytes are at offsets 0, 1, 2 and so on. -/
theorem Wf.offsets_eq {log : List Event} (h : Wf log) : offsets log = List.range (size log) := by
  induction h with
  | nil => rfl
  | @step log op _ ih =>
    have hs : size (push log op) = size log + match op with | .byte _ => 1 | .marker => 0 := by
      cases op <;> simp [size, offsets_push]
    rw [offsets_push, hs, ih]
    cases op <;> simp [List.range_succ]

/-- The markers are numbered 1, 2, 3 and so on. -/
theorem Wf.seqs_eq {log : List Event} (h : Wf log) :
    (markers log).map (·.1) = List.range' 1 (latest log) := by
  induction h with
  | nil => rfl
  | @step log op _ ih =>
    have hl : latest (push log op) = latest log + match op with | .byte _ => 0 | .marker => 1 := by
      cases op <;> simp [latest, markers_push]
    rw [markers_push, hl]
    cases op
    · simpa using ih
    · simp only [List.map_append, ih, List.map_cons, List.map_nil]
      rw [List.range'_concat]
      simp [latest, Nat.add_comm]

theorem Wf.byte_lt {log : List Event} (h : Wf log) {o v : Nat} (he : .byte o v ∈ log) :
    o < size log := by
  have : o ∈ offsets log := List.mem_filterMap.mpr ⟨_, he, rfl⟩
  rw [h.offsets_eq] at this
  simpa using this

theorem Wf.byte_mem {log : List Event} (h : Wf log) {o : Nat} (ho : o < size log) :
    ∃ v, .byte o v ∈ log := by
  have : o ∈ offsets log := by rw [h.offsets_eq]; simpa using ho
  obtain ⟨e, he, hf⟩ := List.mem_filterMap.mp this
  cases e with
  | byte o' v => simp at hf; subst hf; exact ⟨v, he⟩
  | marker => simp at hf

theorem Wf.marker_le {log : List Event} (h : Wf log) {q o : Nat} (he : .marker q o ∈ log) :
    q ≤ latest log := by
  have : (q, o) ∈ markers log := List.mem_filterMap.mpr ⟨_, he, rfl⟩
  have : q ∈ (markers log).map (·.1) := List.mem_map.mpr ⟨_, this, rfl⟩
  rw [h.seqs_eq] at this
  simp [List.mem_range'] at this
  omega

/-! ## The window serves what the stream would -/

/-- When the server retains a cursor, the window has everything the
stream from it holds. -/
theorem servedRetained_eq (s : Stream) (c : Cursor) (h : retains s c) :
    servedRetained s c = served s.log c := by
  unfold retains at h
  simp only [Bool.and_eq_true, decide_eq_true_eq, List.all_eq_true] at h
  obtain ⟨⟨⟨hstart, -⟩, -⟩, hm⟩ := h
  simp only [servedRetained, served, List.filter_filter]
  apply List.filter_congr
  intro e he
  cases e with
  | byte o v =>
    simp only [after, retained]
    by_cases ho : c.offset ≤ o <;> simp [ho] <;> omega
  | marker q o =>
    simp only [after, retained]
    by_cases hq : c.marker < q
    · have := hm (q, o) (List.mem_filterMap.mpr ⟨_, he, rfl⟩)
      simp_all
    · simp [hq]

/-- When it does not, something the cursor needs is gone. -/
theorem not_retains (s : Stream) (c : Cursor) (hw : Wf s.log) (hc : c.offset ≤ size s.log)
    (hl : c.marker ≤ latest s.log) (h : retains s c = false) :
    ∃ e ∈ served s.log c, retained s e = false := by
  unfold retains at h
  by_cases h1 : start s ≤ c.offset
  · have hall : (markers s.log).all
        (fun m => !decide (c.marker < m.1) || (retainedMarkers s).contains m) = false := by
      simpa [h1, hc, hl] using h
    obtain ⟨m, hm, hf⟩ := List.all_eq_false.mp hall
    obtain ⟨e, he, hfe⟩ := List.mem_filterMap.mp hm
    cases e with
    | byte => simp at hfe
    | marker q o =>
      simp only [Option.some.injEq] at hfe
      subst hfe
      refine ⟨_, List.mem_filter.mpr ⟨he, ?_⟩, ?_⟩ <;> simp_all [after, retained]
  · obtain ⟨v, hv⟩ := hw.byte_mem (o := c.offset) (by unfold start at h1; omega)
    exact ⟨_, List.mem_filter.mpr ⟨hv, by simp [after]⟩, by simp [retained]; omega⟩

/-- A cursor at an offset with `seqAt`'s marker is either the one there,
or not retained, so a resume from it gets a snapshot: it never skips a
marker. -/
theorem seqAt_retains (s : Stream) (o : Nat) (hw : Wf s.log) (h : retains s ⟨o, seqAt s o⟩) :
    seqAt s o = reflected s.log o := by
  unfold seqAt at h ⊢
  split
  · rfl
  · next hall =>
    simp only [hall] at h
    simp only [List.all_eq_true, Classical.not_forall] at hall
    obtain ⟨m, hm, hf⟩ := hall
    unfold retains at h
    simp only [Bool.and_eq_true, decide_eq_true_eq, List.all_eq_true] at h
    have hq : m.1 ∈ (markers s.log).map (·.1) := List.mem_map.mpr ⟨m, hm, rfl⟩
    rw [hw.seqs_eq] at hq
    have := h.2 m hm
    simp [List.mem_range'_1] at hq hf this
    simp_all

/-! ## Nothing skipped, nothing repeated -/

theorem offsets_served (log : List Event) (c : Cursor) :
    offsets (served log c) = (offsets log).filter (c.offset ≤ ·) := by
  induction log with
  | nil => rfl
  | cons e log ih =>
    cases e with
    | byte o v => by_cases ho : c.offset ≤ o <;> simp_all [served, after, offsets]
    | marker q o => by_cases hq : c.marker < q <;> simp_all [served, after, offsets]

theorem markers_served (log : List Event) (c : Cursor) :
    markers (served log c) = (markers log).filter (c.marker < ·.1) := by
  induction log with
  | nil => rfl
  | cons e log ih =>
    cases e with
    | byte o v => by_cases ho : c.offset ≤ o <;> simp_all [served, after, markers]
    | marker q o => by_cases hq : c.marker < q <;> simp_all [served, after, markers]

theorem range_filter (n k : Nat) : (List.range n).filter (k ≤ ·) = List.range' k (n - k) := by
  induction n with
  | zero => simp
  | succ n ih =>
    rw [List.range_succ, List.filter_append, ih]
    by_cases hk : k ≤ n
    · rw [show n + 1 - k = (n - k) + 1 by omega, List.range'_concat]
      simp [hk, show k + (n - k) = n by omega]
    · simp [hk, show n - k = 0 by omega, show n + 1 - k = 0 by omega]

theorem range'_filter (n k : Nat) :
    (List.range' 1 n).filter (k < ·) = List.range' (k + 1) (n - k) := by
  induction n with
  | zero => simp
  | succ n ih =>
    rw [List.range'_concat, List.filter_append, ih]
    by_cases hk : k ≤ n
    · rw [show n + 1 - k = (n - k) + 1 by omega, List.range'_concat]
      simp [show k < 1 + n by omega, show k + 1 + (n - k) = 1 + n by omega]
    · simp [show ¬ k < 1 + n by omega, show n - k = 0 by omega, show n + 1 - k = 0 by omega]

/-- The bytes served from a cursor are those at its offset and every one
after, once each and in order. -/
theorem offsets_served_eq {log : List Event} (h : Wf log) (c : Cursor) :
    offsets (served log c) = List.range' c.offset (size log - c.offset) := by
  rw [offsets_served, h.offsets_eq, range_filter]

/-- The markers served are those above its marker, once each and in
order. -/
theorem seqs_served_eq {log : List Event} (h : Wf log) (c : Cursor) :
    (markers (served log c)).map (·.1) = List.range' (c.marker + 1) (latest log - c.marker) := by
  rw [markers_served, ← range'_filter, ← h.seqs_eq, List.filter_map]
  rfl

/-- No byte comes before a marker at or before its offset. -/
def ordered : Event → Event → Prop
  | .byte o _, .marker _ o' => o < o'
  | _, _ => True

theorem Wf.ordered {log : List Event} (h : Wf log) : log.Pairwise ordered := by
  induction h with
  | nil => exact .nil
  | @step log op hw ih =>
    cases op <;> simp only [push, List.pairwise_append, ih, List.pairwise_singleton,
      List.mem_singleton, true_and] <;> intro a ha b hb <;> subst hb
    · cases a <;> trivial
    · cases a with
      | byte o v => exact hw.byte_lt ha
      | marker => trivial

/-- Each marker is served before any byte at or past its offset. -/
theorem served_ordered {log : List Event} (h : Wf log) (c : Cursor) :
    (served log c).Pairwise ordered :=
  h.ordered.sublist List.filter_sublist

/-! ## From one serve to the next -/

theorem served_push {log : List Event} (h : Wf log) (op : Op) (c : Cursor)
    (hc : c.offset ≤ size log ∧ c.marker ≤ latest log) :
    served (push log op) c = served log c ++ served (push log op) (next log) := by
  have hn : served log (next log) = [] := by
    simp only [served, List.filter_eq_nil_iff]
    intro e he
    cases e with
    | byte o v => simp [after, next, Nat.not_le.mpr (h.byte_lt he)]
    | marker q o => simp [after, next, Nat.not_lt.mpr (h.marker_le he)]
  simp only [served] at hn
  cases op <;> simp only [push, served, List.filter_append, hn] <;> simp [after, next] <;> omega

theorem size_push_ge (log : List Event) (op : Op) : size log ≤ size (push log op) := by
  cases op <;> simp [size, offsets_push]

theorem latest_push_ge (log : List Event) (op : Op) : latest log ≤ latest (push log op) := by
  cases op <;> simp [latest, markers_push]

/-- Serving a cursor, then serving where it stopped once more has
happened, sends the stream from the cursor: nothing skipped, nothing
repeated. -/
theorem served_continues (ops : List Op) {log : List Event} (h : Wf log) (c : Cursor)
    (hc : c.offset ≤ size log ∧ c.marker ≤ latest log) :
    served (ops.foldl push log) c = served log c ++ served (ops.foldl push log) (next log) := by
  induction ops generalizing log c with
  | nil =>
    simp only [List.foldl_nil]
    rw [show served log (next log) = [] from ?_, List.append_nil]
    simp only [served, List.filter_eq_nil_iff]
    intro e he
    cases e with
    | byte o v => simp [after, next, Nat.not_le.mpr (h.byte_lt he)]
    | marker q o => simp [after, next, Nat.not_lt.mpr (h.marker_le he)]
  | cons op ops ih =>
    simp only [List.foldl_cons]
    have hw := Wf.step op h
    have hs := size_push_ge log op
    have hl := latest_push_ge log op
    rw [ih hw c ⟨by omega, by omega⟩, ih hw (next log) ⟨by simp [next]; omega, by simp [next]; omega⟩,
      served_push h op c hc, List.append_assoc]

end Clankermux.Stream
