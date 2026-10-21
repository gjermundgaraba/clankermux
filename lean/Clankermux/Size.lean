/-!
# Size policies

PROTOCOL.md, "Size", and the writable `OPEN`'s viewport ("OPEN"): the
attachments' recorded viewports, the policy (`latest`, `smallest`,
`fixed`), the owner under `latest`, and `resize`. A `VIEWPORT` is
recorded at once and applied later, coalesced; which report is applied
when is the session's and left out, as are a session that is not
running, the profile and the cell size.
-/

namespace Clankermux.Size

inductive Policy where
  | latest | smallest | fixed
  deriving DecidableEq, Repr

/-- Columns and rows. -/
abbrev Dims := Nat × Nat

structure Sizing where
  policy : Policy
  owner : Option Nat
  size : Dims
  /-- The open attachments, oldest first, with their latest viewport. -/
  attached : List (Nat × Option Dims)

/-- A new session's: `latest`, no attachments. -/
def new (size : Dims) : Sizing := ⟨.latest, none, size, []⟩

inductive Source where
  | attachment (id : Nat)
  | resize

def isOpen (st : Sizing) (id : Nat) : Bool := st.attached.any (·.1 == id)

def recorded (st : Sizing) : List Dims := st.attached.filterMap (·.2)

/-- The smallest columns and, separately, the smallest rows among the
recorded viewports. -/
def smallest (st : Sizing) : Option Dims :=
  match ((recorded st).map (·.1)).min?, ((recorded st).map (·.2)).min? with
  | some cols, some rows => some (cols, rows)
  | _, _ => none

def record (st : Sizing) (id : Nat) (d : Dims) : Sizing :=
  { st with attached := st.attached.map fun (a, v) => if a = id then (a, some d) else (a, v) }

/-- A report applied: `resize` sets its size under every policy and
leaves no owner; under `latest` a report sets its size and its
attachment owns it while open; under `smallest` the size is the smallest
recorded, unchanged while none is; under `fixed` it is unchanged. -/
def apply (st : Sizing) (src : Source) (d : Dims) : Sizing :=
  match src, st.policy with
  | .resize, _ => { st with size := d, owner := none }
  | .attachment id, .latest =>
    { st with size := d, owner := if isOpen st id then some id else none }
  | .attachment _, .smallest => { st with size := (smallest st).getD st.size }
  | .attachment _, .fixed => st

/-- Under `smallest`, the size follows the recorded viewports. -/
def follow (st : Sizing) : Sizing :=
  match st.policy with
  | .smallest => { st with size := (smallest st).getD st.size }
  | _ => st

/-- `OPEN`: a writable one's viewport is a report applied at once. -/
def openAttachment (st : Sizing) (id : Nat) (viewport : Option Dims) : Sizing :=
  let opened := { st with attached := st.attached ++ [(id, none)] }
  match viewport with
  | none => opened
  | some d => apply (record opened id d) (.attachment id) d

/-- An attachment closes: it no longer owns the size, and under
`smallest` the size follows those left. -/
def close (st : Sizing) (id : Nat) : Sizing :=
  follow { st with
    attached := st.attached.filter (·.1 != id)
    owner := if st.owner = some id then none else st.owner }

def resize (st : Sizing) (d : Dims) : Sizing := apply st .resize d

/-- Setting a policy clears the owner; `smallest` applies at once. -/
def setPolicy (st : Sizing) (p : Policy) : Sizing :=
  follow { st with policy := p, owner := none }

/-! ## The owner -/

/-- An owner is an open attachment, and only under `latest`. -/
def Inv (st : Sizing) : Prop :=
  ∀ id, st.owner = some id → isOpen st id ∧ st.policy = .latest

theorem isOpen_record (st : Sizing) (id a : Nat) (d : Dims) :
    isOpen (record st id d) a = isOpen st a := by
  simp only [isOpen, record, List.any_map]
  congr 1
  funext x
  simp only [Function.comp]
  split <;> rfl

theorem follow_owner (st : Sizing) : (follow st).owner = st.owner := by
  unfold follow; split <;> rfl

theorem follow_policy (st : Sizing) : (follow st).policy = st.policy := by
  unfold follow; split <;> rfl

theorem follow_attached (st : Sizing) : (follow st).attached = st.attached := by
  unfold follow; split <;> rfl

theorem inv_follow (st : Sizing) (h : Inv st) : Inv (follow st) := by
  intro a ha
  rw [follow_owner] at ha
  simpa [isOpen, follow_attached, follow_policy] using h a ha

theorem inv_new (size : Dims) : Inv (new size) := by
  intro id h; simp [new] at h

theorem inv_apply (st : Sizing) (src : Source) (d : Dims) (h : Inv st) : Inv (apply st src d) := by
  intro id hid
  cases src with
  | resize => simp [apply] at hid
  | attachment a =>
    cases hp : st.policy <;> simp only [apply, hp] at hid ⊢
    · split at hid
      · next ho => simp at hid; subst hid; constructor <;> simp_all [isOpen]
      · simp at hid
    · have := h id hid; simp_all [isOpen]
    · have := h id hid; simp_all [isOpen]

theorem inv_record (st : Sizing) (id : Nat) (d : Dims) (h : Inv st) : Inv (record st id d) := by
  intro a ha
  have := h a ha
  rw [isOpen_record]
  exact this

theorem inv_open (st : Sizing) (id : Nat) (v : Option Dims) (h : Inv st) :
    Inv (openAttachment st id v) := by
  have h' : Inv { st with attached := st.attached ++ [(id, none)] } := by
    intro a ha
    obtain ⟨ho, hp⟩ := h a ha
    refine ⟨?_, hp⟩
    simp only [isOpen, List.any_append] at ho ⊢
    simp [ho]
  unfold openAttachment
  cases v with
  | none => exact h'
  | some d => exact inv_apply _ _ _ (inv_record _ _ _ h')

theorem inv_close (st : Sizing) (id : Nat) (h : Inv st) : Inv (close st id) := by
  apply inv_follow
  intro a ha
  simp only at ha
  split at ha
  · simp at ha
  · next hne =>
    obtain ⟨hop, hpl⟩ := h a ha
    refine ⟨?_, hpl⟩
    simp only [isOpen, List.any_filter, List.any_eq_true, Bool.and_eq_true, bne_iff_ne, ne_eq,
      beq_iff_eq] at hop ⊢
    obtain ⟨x, hx, rfl⟩ := hop
    exact ⟨x, hx, fun he => hne (by rw [ha, he]), rfl⟩

theorem inv_resize (st : Sizing) (d : Dims) (h : Inv st) : Inv (resize st d) :=
  inv_apply st .resize d h

theorem inv_setPolicy (st : Sizing) (p : Policy) : Inv (setPolicy st p) := by
  apply inv_follow
  intro a ha
  simp at ha

/-! ## What each policy does -/

/-- The smallest is at most every recorded viewport, in each dimension,
and each of its dimensions is some recorded viewport's. -/
theorem smallest_spec (st : Sizing) (c r : Nat) (h : smallest st = some (c, r)) :
    (∀ v ∈ recorded st, c ≤ v.1 ∧ r ≤ v.2) ∧
      (∃ v ∈ recorded st, v.1 = c) ∧ (∃ v ∈ recorded st, v.2 = r) := by
  unfold smallest at h
  split at h
  · next cols rows hc hr =>
    simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    rw [List.min?_eq_some_iff] at hc hr
    refine ⟨fun v hv => ⟨hc.2 _ (List.mem_map.mpr ⟨v, hv, rfl⟩),
      hr.2 _ (List.mem_map.mpr ⟨v, hv, rfl⟩)⟩, ?_, ?_⟩
    · obtain ⟨v, hv, he⟩ := List.mem_map.mp hc.1; exact ⟨v, hv, he⟩
    · obtain ⟨v, hv, he⟩ := List.mem_map.mp hr.1; exact ⟨v, hv, he⟩
  · simp at h

theorem smallest_none (st : Sizing) : smallest st = none ↔ recorded st = [] := by
  constructor
  · intro h
    refine Classical.byContradiction fun hne => ?_
    obtain ⟨c, hc⟩ := Option.ne_none_iff_exists'.mp
      (mt List.min?_eq_none_iff.mp (by simpa using hne : (recorded st).map (·.1) ≠ []))
    obtain ⟨r, hr⟩ := Option.ne_none_iff_exists'.mp
      (mt List.min?_eq_none_iff.mp (by simpa using hne : (recorded st).map (·.2) ≠ []))
    simp [smallest, hc, hr] at h
  · intro h
    simp [smallest, h]

theorem recorded_follow (st : Sizing) : recorded (follow st) = recorded st := by
  simp [recorded, follow_attached]

theorem smallest_follow (st : Sizing) : smallest (follow st) = smallest st := by
  simp [smallest, recorded_follow]

theorem follow_smallest (st : Sizing) (hp : st.policy = .smallest) (hr : recorded st ≠ []) :
    some (follow st).size = smallest st := by
  obtain ⟨v, hv⟩ := Option.ne_none_iff_exists'.mp (mt (smallest_none st).mp hr)
  simp [follow, hp, hv]

/-- Under `smallest` a report applies the smallest recorded. -/
theorem apply_smallest (st : Sizing) (id : Nat) (d : Dims) (hp : st.policy = .smallest)
    (hr : recorded st ≠ []) : some (apply st (.attachment id) d).size = smallest st := by
  obtain ⟨v, hv⟩ := Option.ne_none_iff_exists'.mp (mt (smallest_none st).mp hr)
  simp [apply, hp, hv]

/-- Under `fixed` reports are recorded and not applied. -/
theorem apply_fixed (st : Sizing) (id : Nat) (d : Dims) (hp : st.policy = .fixed) :
    (apply st (.attachment id) d).size = st.size := by
  simp [apply, hp]

theorem open_fixed (st : Sizing) (id : Nat) (v : Option Dims) (hp : st.policy = .fixed) :
    (openAttachment st id v).size = st.size := by
  unfold openAttachment
  cases v <;> simp [apply, record, hp]

/-- Under `latest` the report's size, owned by its attachment while it is
open. -/
theorem apply_latest (st : Sizing) (id : Nat) (d : Dims) (hp : st.policy = .latest) :
    (apply st (.attachment id) d).size = d ∧
      ((apply st (.attachment id) d).owner = some id ↔ isOpen st id) := by
  simp [apply, hp]

/-- `resize` sets its size under every policy, with no owner. -/
theorem resize_spec (st : Sizing) (d : Dims) :
    (resize st d).size = d ∧ (resize st d).owner = none := by
  simp [resize, apply]

/-- Closing under `smallest` recomputes the size from those left. -/
theorem close_smallest (st : Sizing) (id : Nat) (hp : st.policy = .smallest)
    (hr : recorded (close st id) ≠ []) :
    some (close st id).size = smallest (close st id) := by
  unfold close at hr ⊢
  rw [smallest_follow]
  rw [recorded_follow] at hr
  exact follow_smallest _ hp hr

/-- Setting a policy clears the owner; setting `smallest` applies it. -/
theorem setPolicy_spec (st : Sizing) (p : Policy) :
    (setPolicy st p).owner = none ∧ (setPolicy st p).policy = p ∧
      (p = .smallest → recorded st ≠ [] → some (setPolicy st p).size = smallest st) := by
  refine ⟨by simp [setPolicy, follow_owner], by simp [setPolicy, follow_policy], ?_⟩
  intro hp hr
  exact follow_smallest { st with policy := p, owner := none } hp hr

end Clankermux.Size
