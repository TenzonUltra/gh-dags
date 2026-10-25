/-!
# Layouts as parent pointers

The physical layout is a parent function: each change sits on another change,
or on trunk (`none`). `Anc par a v` says `a` is a physical ancestor of `v`;
`LAnc deps a v` says `a` is a logical ancestor. The invariant `Inv` is "every
dep is a physical ancestor". A stack is a list where each element sits on the
one before it (`predIn`), and the chain lemma says ancestry on such a list is
exactly "comes before".
-/

namespace GhDags

variable {N : Type}

/-- `a` is reached by following parents up from `v`. -/
inductive Anc (par : N → Option N) : N → N → Prop
  | parent {a v} : par v = some a → Anc par a v
  | step {a b v} : par v = some b → Anc par a b → Anc par a v

/-- Following parents up from `v` reaches trunk. -/
inductive Rooted (par : N → Option N) : N → Prop
  | trunk {v} : par v = none → Rooted par v
  | step {v p} : par v = some p → Rooted par p → Rooted par v

/-- `a` is reached by following deps from `v`. -/
inductive LAnc (deps : N → List N) : N → N → Prop
  | dep {a v} : a ∈ deps v → LAnc deps a v
  | step {a b v} : b ∈ deps v → LAnc deps a b → LAnc deps a v

/-- The invariant: every dependency is a physical ancestor. -/
def Inv (par : N → Option N) (deps : N → List N) : Prop :=
  ∀ v d, d ∈ deps v → Anc par d v

section
variable {par : N → Option N} {deps : N → List N}

theorem Anc.iff {a v : N} : Anc par a v ↔ ∃ b, par v = some b ∧ (a = b ∨ Anc par a b) := by
  constructor
  · intro h
    cases h with
    | parent h => exact ⟨_, h, Or.inl rfl⟩
    | step h h' => exact ⟨_, h, Or.inr h'⟩
  · rintro ⟨b, hb, rfl | h⟩
    · exact .parent hb
    · exact .step hb h

theorem Anc.trans {a b v : N} (h1 : Anc par a b) (h2 : Anc par b v) : Anc par a v := by
  induction h2 with
  | parent h => exact .step h h1
  | step h _ ih => exact .step h ih

theorem LAnc.trans {a b v : N} (h1 : LAnc deps a b) (h2 : LAnc deps b v) : LAnc deps a v := by
  induction h2 with
  | dep h => exact .step h h1
  | step h _ ih => exact .step h ih

/-- Logical ancestors are physical ancestors, given the invariant. -/
theorem Inv.lanc (hinv : Inv par deps) {a v : N} (h : LAnc deps a v) : Anc par a v := by
  induction h with
  | dep h => exact hinv _ _ h
  | step hb _ ih => exact ih.trans (hinv _ _ hb)

/-- A rooted layout has no cycles. -/
theorem Rooted.not_anc_self {v : N} (h : Rooted par v) : ¬ Anc par v v := by
  induction h with
  | trunk h =>
    intro ha
    obtain ⟨b, hb, _⟩ := Anc.iff.mp ha
    simp [h] at hb
  | step hp _ ih =>
    intro ha
    obtain ⟨b, hb, hab⟩ := Anc.iff.mp ha
    rw [hp] at hb
    cases hb
    rcases hab with rfl | hab
    · exact ih (.parent hp)
    · exact ih ((Anc.parent hp).trans hab)
end

/-! ## Lists where each element sits on the previous one -/

theorem nodup_split_unique {L s1 s2 u1 u2 : List N} {b : N} (hnd : L.Nodup)
    (h1 : L = s1 ++ b :: u1) (h2 : L = s2 ++ b :: u2) : s1 = s2 ∧ u1 = u2 := by
  subst h1
  induction s1 generalizing s2 with
  | nil =>
    cases s2 with
    | nil => simp_all
    | cons c s2 =>
      simp only [List.nil_append, List.cons_append, List.cons.injEq] at h2
      obtain ⟨rfl, rfl⟩ := h2
      simp at hnd
  | cons a s1 ih =>
    cases s2 with
    | nil =>
      simp only [List.nil_append, List.cons_append, List.cons.injEq] at h2
      obtain ⟨rfl, rfl⟩ := h2
      simp at hnd
    | cons c s2 =>
      simp only [List.cons_append, List.cons.injEq] at h2
      obtain ⟨rfl, h2⟩ := h2
      have := ih (List.nodup_cons.mp hnd).2 h2
      simp [this]

variable [DecidableEq N]

/-- The element just before `v` in the list, if any. -/
def predIn : List N → N → Option N
  | a :: b :: t, v => if v = b then some a else predIn (b :: t) v
  | _, _ => none

/-- `a` comes somewhere before `v` in `L`. -/
def Before (L : List N) (a v : N) : Prop := ∃ s t u, L = s ++ a :: t ++ v :: u

theorem predIn_of_not_mem_tail {L : List N} {v : N} (h : v ∉ L.tail) : predIn L v = none := by
  match L with
  | [] => rfl
  | [_] => rfl
  | a :: b :: t =>
    simp only [List.tail, List.mem_cons, not_or] at h
    simp only [predIn, h.1, ite_false]
    exact predIn_of_not_mem_tail (by simpa using h.2)

theorem predIn_mid {s : List N} {u v : N} {t : List N} (hnd : (s ++ u :: v :: t).Nodup) :
    predIn (s ++ u :: v :: t) v = some u := by
  induction s with
  | nil => simp [predIn]
  | cons a s ih =>
    have hnd' : (s ++ u :: v :: t).Nodup := (List.nodup_cons.mp hnd).2
    cases s with
    | nil =>
      have : v ≠ u := by intro h; subst h; simp at hnd
      simp [predIn, this] at ih ⊢
    | cons b s =>
      have : v ≠ b := by
        intro h; subst h
        have := (List.nodup_cons.mp hnd').1
        simp at this
      simp only [List.cons_append, predIn, this, ite_false] at ih ⊢
      exact ih hnd'

/-- `predIn` finds exactly the element right before `v`. -/
theorem predIn_eq_some {L : List N} {v a : N} (h : predIn L v = some a) :
    ∃ s t, L = s ++ a :: v :: t := by
  match L with
  | [] => simp [predIn] at h
  | [_] => simp [predIn] at h
  | x :: y :: t =>
    simp only [predIn] at h
    split at h
    · cases h; rename_i hv; subst hv; exact ⟨[], t, rfl⟩
    · obtain ⟨s, t', he⟩ := predIn_eq_some h
      exact ⟨x :: s, t', by simp [he]⟩

omit [DecidableEq N] in
theorem before_iff_mem_prefix {L l1 l2 : List N} {a v : N} (hnd : L.Nodup)
    (hL : L = l1 ++ v :: l2) : Before L a v ↔ a ∈ l1 := by
  constructor
  · rintro ⟨s, t, u, he⟩
    have := (nodup_split_unique (s2 := s ++ a :: t) (u2 := u) hnd hL (by simp [he])).1
    simp [this]
  · intro ha
    obtain ⟨s, t, rfl⟩ := List.append_of_mem ha
    exact ⟨s, t, l2, by simp [hL]⟩

section chain
variable {par : N → Option N} {L : List N}

theorem anc_before (hnd : L.Nodup) (hpar : ∀ v ∈ L, par v = predIn L v) {a v : N}
    (h : Anc par a v) : v ∈ L → Before L a v := by
  induction h with
  | @parent v h =>
    intro hv
    rw [hpar v hv] at h
    obtain ⟨s, t, he⟩ := predIn_eq_some h
    exact ⟨s, [], t, by simp [he]⟩
  | @step b v h _ ih =>
    intro hv
    rw [hpar v hv] at h
    obtain ⟨s, t, he⟩ := predIn_eq_some h
    obtain ⟨s', t', u', he'⟩ := ih (by simp [he])
    have hu := (nodup_split_unique (s1 := s' ++ a :: t') (u1 := u') (s2 := s) (u2 := v :: t) hnd (by simp [he']) he).2
    exact ⟨s', t' ++ [b], t, by rw [he', hu]; simp⟩

theorem before_anc (hnd : L.Nodup) (hpar : ∀ v ∈ L, par v = predIn L v) :
    ∀ (t s : List N) (a v : N) (u : List N), L = s ++ a :: t ++ v :: u → Anc par a v := by
  intro t
  induction t with
  | nil =>
    intro s a v u he
    have he2 : L = s ++ a :: v :: u := by simp [he]
    apply Anc.parent
    rw [hpar v (by simp [he2]), he2]
    exact predIn_mid (he2 ▸ hnd)
  | cons b t ih =>
    intro s a v u he
    have he2 : L = s ++ a :: b :: (t ++ v :: u) := by simp [he]
    have hb : par b = some a := by
      rw [hpar b (by simp [he2]), he2]
      exact predIn_mid (he2 ▸ hnd)
    exact (Anc.parent hb).trans (ih (s ++ [a]) b v u (by simp [he]))

/-- On a chain, ancestors are exactly the elements before. -/
theorem chain_anc (hnd : L.Nodup) (hpar : ∀ v ∈ L, par v = predIn L v) {a v : N}
    (hv : v ∈ L) : Anc par a v ↔ Before L a v := by
  constructor
  · intro h; exact anc_before hnd hpar h hv
  · rintro ⟨s, t, u, he⟩; exact before_anc hnd hpar t s a v u he

/-- Every element of a chain reaches trunk, given the chain's bottom does. -/
theorem chain_rooted (hnd : L.Nodup) (hpar : ∀ v ∈ L, par v = predIn L v) {v : N}
    (hv : v ∈ L) : Rooted par v := by
  obtain ⟨s, t, he⟩ := List.append_of_mem hv
  induction h : s.length using Nat.strongRecOn generalizing s v t with
  | _ n ih =>
    rcases List.eq_nil_or_concat s with rfl | ⟨s', u, rfl⟩
    · apply Rooted.trunk
      rw [hpar v hv]
      apply predIn_of_not_mem_tail
      subst he
      simpa using (List.nodup_cons.mp hnd).1
    · have he' : L = s' ++ u :: v :: t := by simp [he]
      have hp : par v = some u := by rw [hpar v hv, he']; exact predIn_mid (by rw [← he']; exact hnd)
      exact .step hp (ih s'.length (by simp [← h]) (by rw [he']; simp) s' (t := v :: t) he' rfl)
end chain

end GhDags
