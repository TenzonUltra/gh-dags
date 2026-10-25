import GhDags.Chain

/-!
# Hoisting (`land`) is correct

`land x` takes `x`'s root path `P` (trunk up to `x`, possibly running through
spurs) and makes it one trunk stack, the *chain*:

    C = (P.filter need) ++ (P.filter (not need))

where `need v` means `v = x` or `x` logically depends on `v`. Whatever sat
directly above `x` in its stack (`cont`) goes on top of the chain. Every other
branch hanging directly off the path (a *side root*) is re-anchored onto some
chain node at or above everything that branch needs from the path. Everything
else keeps its parent.

We prove, for any layout satisfying the invariant:

* `newPar_inv`: the result satisfies the invariant (every dep is a physical
  ancestor);
* `newPar_exact`: `x`'s physical ancestors are exactly its logical ancestors,
  i.e. it has no false dependencies left, so it can merge as a prefix;
* `newPar_rooted` / `newPar_acyclic`: every change still reaches trunk, so
  the result has no cycles.

The re-anchoring rule is a hypothesis (`anchor_mem`, `anchor_covers`), not a
fixed function, so the theorem covers any implementation of it. The Rust code
takes the highest needed node (`max_by_key`), which satisfies it. Sums
(parked stacks) are outside this model.
-/

namespace GhDags.Hoist

open GhDags

variable {N : Type} [DecidableEq N]

/-- The new trunk stack: what `x` needs, then the rest of its path, each in
their old order. -/
def chain (need : N → Bool) (P : List N) : List N :=
  P.filter need ++ P.filter (fun v => !need v)

/-- Everything hoisting looks at. -/
structure Setup (N : Type) where
  par : N → Option N
  deps : N → List N
  x : N
  /-- `x`'s root path, bottom first. -/
  P : List N
  /-- `x`'s successor in its stack, if any. -/
  cont : Option N
  /-- `x` and what it logically depends on. -/
  need : N → Bool
  /-- Where each branch hanging off the path goes. -/
  anchor : N → N

namespace Setup

variable (s : Setup N)

def C : List N := chain s.need s.P

/-- Hangs directly off the path, isn't on it, and isn't `x`'s successor. -/
def SideRoot (v : N) : Prop := v ∉ s.P ∧ s.cont ≠ some v ∧ ∃ p, s.par v = some p ∧ p ∈ s.P

/-- What the branch rooted at `e` needs: the logical ancestors of anything in it. -/
def Needs (e d : N) : Prop := ∃ w, (w = e ∨ Anc s.par e w) ∧ LAnc s.deps d w

/-- The layout after hoisting `x`. -/
def newPar (v : N) : Option N :=
  if v ∈ s.P then predIn s.C v
  else if s.cont = some v then s.C.getLast?
  else match s.par v with
    | some p => if p ∈ s.P then some (s.anchor v) else some p
    | none => none

/-- What hoisting assumes about the layout before. -/
structure Hyp : Prop where
  rooted : ∀ v, Rooted s.par v
  inv : Inv s.par s.deps
  nodup : s.P.Nodup
  path : ∀ v ∈ s.P, s.par v = predIn s.P v
  last : s.P.getLast? = some s.x
  need : ∀ v, s.need v = true ↔ (v = s.x ∨ LAnc s.deps v s.x)
  cont : ∀ c, s.cont = some c → s.par c = some s.x ∧ c ∉ s.P
  /-- The re-anchoring rule: onto a path node... -/
  anchor_mem : ∀ v, s.SideRoot v → s.anchor v ∈ s.P
  /-- ...at or above everything the branch needs from the path. -/
  anchor_covers : ∀ v, s.SideRoot v → ∀ d ∈ s.P, s.Needs v d →
    d = s.anchor v ∨ Before s.C d (s.anchor v)

end Setup

/-! ## Lists -/

omit [DecidableEq N] in
theorem before_mem {L : List N} {a v : N} (h : Before L a v) : a ∈ L ∧ v ∈ L := by
  obtain ⟨s, t, u, rfl⟩ := h
  simp

omit [DecidableEq N] in
theorem before_filter {L : List N} {a v : N} {f : N → Bool} (h : Before L a v)
    (ha : f a = true) (hv : f v = true) : Before (L.filter f) a v := by
  obtain ⟨s, t, u, rfl⟩ := h
  exact ⟨s.filter f, t.filter f, u.filter f, by simp [List.filter_append, ha, hv]⟩

omit [DecidableEq N] in
theorem before_append_left {A B : List N} {a v : N} (h : Before A a v) : Before (A ++ B) a v := by
  obtain ⟨s, t, u, rfl⟩ := h
  exact ⟨s, t, u ++ B, by simp⟩

omit [DecidableEq N] in
theorem before_append_right {A B : List N} {a v : N} (h : Before B a v) : Before (A ++ B) a v := by
  obtain ⟨s, t, u, rfl⟩ := h
  exact ⟨A ++ s, t, u, by simp⟩

omit [DecidableEq N] in
theorem before_append_across {A B : List N} {a v : N} (ha : a ∈ A) (hv : v ∈ B) :
    Before (A ++ B) a v := by
  obtain ⟨s, t, rfl⟩ := List.append_of_mem ha
  obtain ⟨s', t', rfl⟩ := List.append_of_mem hv
  exact ⟨s, t ++ s', t', by simp⟩

namespace Setup

variable {s : Setup N}

omit [DecidableEq N] in
theorem mem_C {v : N} : v ∈ s.C ↔ v ∈ s.P := by
  simp only [C, chain, List.mem_append, List.mem_filter]
  cases s.need v <;> simp

theorem nodup_C (h : s.Hyp) : s.C.Nodup := by
  simp only [C, chain]
  rw [List.nodup_append]
  refine ⟨h.nodup.sublist List.filter_sublist, h.nodup.sublist List.filter_sublist, ?_⟩
  intro a ha b hb hab
  subst hab
  simp only [List.mem_filter] at ha hb
  simp [ha.2] at hb

theorem newPar_C {v : N} (hv : v ∈ s.C) : s.newPar v = predIn s.C v := by
  simp [newPar, mem_C.mp hv]

theorem x_mem (h : s.Hyp) : s.x ∈ s.P := List.mem_of_getLast? h.last

theorem need_x (h : s.Hyp) : s.need s.x = true := (h.need s.x).mpr (Or.inl rfl)

/-- On the old path, ancestors are exactly the earlier path nodes. -/
theorem path_anc (h : s.Hyp) {a v : N} (hv : v ∈ s.P) : Anc s.par a v ↔ Before s.P a v :=
  chain_anc h.nodup h.path hv

/-- The new chain is a chain. -/
theorem chain_anc_C (h : s.Hyp) {a v : N} (hv : v ∈ s.C) : Anc s.newPar a v ↔ Before s.C a v :=
  chain_anc (nodup_C h) (fun _ hw => newPar_C hw) hv

/-- Ancestors of a path node are on the path. -/
theorem path_closed (h : s.Hyp) {a v : N} (hv : v ∈ s.P) (ha : Anc s.par a v) : a ∈ s.P :=
  (before_mem ((path_anc h hv).mp ha)).1

/-- Ancestry off the path is preserved. -/
theorem lift (h : s.Hyp) {a v : N} (ha : Anc s.par a v) (hna : a ∉ s.P) : Anc s.newPar a v := by
  induction ha with
  | @parent v hp =>
    have hv : v ∉ s.P := fun hv => hna (path_closed h hv (.parent hp))
    have hc : s.cont ≠ some v := by
      intro hc
      have := (h.cont v hc).1
      rw [hp] at this; cases this
      exact hna (x_mem h)
    apply Anc.parent
    simp [newPar, hv, hc, hp, hna]
  | @step b v hp hab ih =>
    have hv : v ∉ s.P := fun hv => hna (path_closed h hv (.step hp hab))
    have hb : b ∉ s.P := fun hb => hna (path_closed h hb hab)
    have hc : s.cont ≠ some v := by
      intro hc
      have := (h.cont v hc).1
      rw [hp] at this; cases this
      exact hb (x_mem h)
    apply Anc.step _ ih
    simp [newPar, hv, hc, hp, hb]

/-- A branch that reaches the path leaves it at some node hanging right off it. -/
theorem exit {d v : N} (hd : d ∈ s.P) (ha : Anc s.par d v) :
    v ∉ s.P → ∃ e, e ∉ s.P ∧ (∃ p, s.par e = some p ∧ p ∈ s.P) ∧ (e = v ∨ Anc s.par e v) := by
  induction ha with
  | @parent v hp => intro hv; exact ⟨v, hv, ⟨_, hp, hd⟩, Or.inl rfl⟩
  | @step b v hp _ ih =>
    intro hv
    by_cases hb : b ∈ s.P
    · exact ⟨v, hv, ⟨b, hp, hb⟩, Or.inl rfl⟩
    · obtain ⟨e, he, hpe, hev⟩ := ih hb
      refine ⟨e, he, hpe, Or.inr ?_⟩
      rcases hev with rfl | hev
      · exact .parent hp
      · exact .step hp hev

theorem C_ne_nil (h : s.Hyp) : s.C ≠ [] := by
  intro hc
  have := mem_C.mpr (x_mem h)
  simp [hc] at this

/-- Wherever a branch leaves the path, what it needs from the path is below it. -/
theorem exit_covered (h : s.Hyp) {e : N} (he : e ∉ s.P) (hpe : ∃ p, s.par e = some p ∧ p ∈ s.P)
    {d : N} (hd : d ∈ s.P) (hn : s.Needs e d) : Anc s.newPar d e := by
  by_cases hc : s.cont = some e
  · -- `x`'s successor: it sits on top of the whole chain.
    obtain ⟨l, hl⟩ : ∃ l, s.C.getLast? = some l := by
      cases hl : s.C.getLast? with
      | none => exact absurd (List.getLast?_eq_none_iff.mp hl) (C_ne_nil h)
      | some l => exact ⟨l, rfl⟩
    have hnp : s.newPar e = some l := by simp [newPar, he, hc, hl]
    obtain ⟨C', hC⟩ := List.getLast?_eq_some_iff.mp hl
    have hdC : d ∈ s.C := mem_C.mpr hd
    rw [hC] at hdC
    rcases List.mem_append.mp hdC with hd' | hd'
    · have hb : Before s.C d l := by
        obtain ⟨u, w, rfl⟩ := List.append_of_mem hd'
        exact ⟨u, w, [], by simp [hC]⟩
      exact .step hnp ((chain_anc_C h (List.mem_of_getLast? hl)).mpr hb)
    · simp at hd'; subst hd'; exact .parent hnp
  · -- A side root: re-anchored at or above what it needs.
    have hsr : s.SideRoot e := ⟨he, hc, hpe⟩
    obtain ⟨p, hp, hpP⟩ := hpe
    have hnp : s.newPar e = some (s.anchor e) := by simp [newPar, he, hc, hp, hpP]
    rcases h.anchor_covers e hsr d hd hn with rfl | hb
    · exact .parent hnp
    · exact .step hnp ((chain_anc_C h (mem_C.mpr (h.anchor_mem e hsr))).mpr hb)

/-! ## The theorems -/

/-- **The invariant survives hoisting.** -/
theorem newPar_inv (h : s.Hyp) : Inv s.newPar s.deps := by
  intro v d hd
  have hold := h.inv v d hd
  by_cases hv : v ∈ s.P
  · -- On the path: the chain keeps every dep below.
    apply (chain_anc_C h (mem_C.mpr hv)).mpr
    have hbP := (path_anc h hv).mp hold
    by_cases hnv : s.need v = true
    · have hnd : s.need d = true := by
        apply (h.need d).mpr; right
        rcases (h.need v).mp hnv with rfl | hvx
        · exact .dep hd
        · exact (LAnc.dep hd).trans hvx
      exact before_append_left (before_filter hbP hnd hnv)
    · by_cases hnd : s.need d = true
      · exact before_append_across (List.mem_filter.mpr ⟨(before_mem hbP).1, hnd⟩)
          (List.mem_filter.mpr ⟨hv, by simpa using hnv⟩)
      · exact before_append_right (before_filter hbP (by simpa using hnd) (by simpa using hnv))
  · by_cases hdP : d ∈ s.P
    · -- Off the path, needing something on it: covered where the branch leaves the path.
      obtain ⟨e, he, hpe, hev⟩ := exit hdP hold hv
      have hn : s.Needs e d := ⟨v, by rcases hev with rfl | hev <;> simp_all, .dep hd⟩
      have hde := exit_covered h he hpe hdP hn
      rcases hev with rfl | hev
      · exact hde
      · exact hde.trans (lift h hev he)
    · exact lift h hold hdP

/-- **`x` ends up with no false dependencies**: its physical ancestors are
exactly its logical ancestors. -/
theorem newPar_exact (h : s.Hyp) (a : N) : Anc s.newPar a s.x ↔ LAnc s.deps a s.x := by
  have hxC : s.x ∈ s.C := mem_C.mpr (x_mem h)
  rw [chain_anc_C h hxC]
  constructor
  · -- Everything before `x` in the chain is in the `need` part.
    intro hb
    obtain ⟨s1, t1, hA⟩ := List.append_of_mem (List.mem_filter.mpr ⟨x_mem h, need_x h⟩)
    have hC : s.C = s1 ++ s.x :: (t1 ++ s.P.filter (fun v => !s.need v)) := by
      simp [C, chain, hA]
    have ha1 : a ∈ s1 := (before_iff_mem_prefix (nodup_C h) hC).mp hb
    have hneed : s.need a = true := by
      have : a ∈ s.P.filter s.need := by rw [hA]; simp [ha1]
      exact (List.mem_filter.mp this).2
    rcases (h.need a).mp hneed with rfl | hl
    · exfalso
      have hnd := nodup_C h
      rw [hC, List.nodup_append] at hnd
      exact hnd.2.2 _ ha1 _ (by simp) rfl
    · exact hl
  · intro hl
    have hbP := (path_anc h (x_mem h)).mp (h.inv.lanc hl)
    exact before_append_left (before_filter hbP ((h.need a).mpr (Or.inr hl)) (need_x h))

/-- **Every change still reaches trunk.** -/
theorem newPar_rooted (h : s.Hyp) (v : N) : Rooted s.newPar v := by
  have hchain : ∀ w ∈ s.C, Rooted s.newPar w :=
    fun _ hw => chain_rooted (nodup_C h) (fun _ hu => newPar_C hu) hw
  induction h.rooted v with
  | @trunk v hp =>
    by_cases hv : v ∈ s.P
    · exact hchain v (mem_C.mpr hv)
    · have hc : s.cont ≠ some v := by
        intro hc; have := (h.cont v hc).1; rw [hp] at this; cases this
      exact .trunk (by simp [newPar, hv, hc, hp])
  | @step v p hp _ ih =>
    by_cases hv : v ∈ s.P
    · exact hchain v (mem_C.mpr hv)
    · by_cases hc : s.cont = some v
      · obtain ⟨l, hl⟩ : ∃ l, s.C.getLast? = some l := by
          cases hl : s.C.getLast? with
          | none => exact absurd (List.getLast?_eq_none_iff.mp hl) (C_ne_nil h)
          | some l => exact ⟨l, rfl⟩
        exact .step (by simp [newPar, hv, hc, hl]) (hchain l (List.mem_of_getLast? hl))
      · by_cases hpP : p ∈ s.P
        · have hsr : s.SideRoot v := ⟨hv, hc, p, hp, hpP⟩
          exact .step (by simp [newPar, hv, hc, hp, hpP])
            (hchain _ (mem_C.mpr (h.anchor_mem v hsr)))
        · exact .step (by simp [newPar, hv, hc, hp, hpP]) ih

/-- **Hoisting never creates a cycle.** -/
theorem newPar_acyclic (h : s.Hyp) (v : N) : ¬ Anc s.newPar v v :=
  (newPar_rooted h v).not_anc_self

end Setup

end GhDags.Hoist
