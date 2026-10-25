/-!
# `submit` never makes GitHub implicitly merge a PR

GitHub marks a pull request merged the moment its base branch contains its
head commit. `submit` both changes PR bases and force-pushes rewritten
branches, so the order of those steps decides whether that can happen. (It did
happen on real GitHub, with push-then-retarget: see `push_first_merges`.)

The order `submit` uses:

1. retarget every PR whose base changes to trunk (one at a time);
2. push every branch in one atomic push;
3. retarget those PRs to their new bases (one at a time).

We model commits only through reachability, and show that every moment this
protocol can pass through is safe, under four assumptions that hold for any
restacked layout (`Assumptions`). Then we show the two obvious alternatives
are unsafe on a two-change reorder.
-/

namespace GhDags.Retarget

/-- Everything `submit` touches. Changes are `V`, commits are `C`, and a base
of `none` is trunk. -/
structure World (V C : Type) where
  /-- `reach a b`: commit `a` is in the history of commit `b`. -/
  reach : C → C → Prop
  trunk : C
  /-- Each change's tip before the push, and after. -/
  old : V → C
  new : V → C
  /-- Each change's base before (the PR's current base), and in the new layout. -/
  oldBase : V → Option V
  newBase : V → Option V

variable {V C : Type}

namespace World

variable (w : World V C)

/-- What a restacked layout guarantees. -/
structure Assumptions : Prop where
  /-- Trunk contains none of the changes' unmerged work, old or new. -/
  trunk_old : ∀ v, ¬ w.reach (w.old v) w.trunk
  trunk_new : ∀ v, ¬ w.reach (w.new v) w.trunk
  /-- Before: each open PR's head is ahead of its base (else GitHub would
  already have marked it merged). -/
  old_ahead : ∀ v p, w.oldBase v = some p → ¬ w.reach (w.old v) (w.old p)
  /-- After the restack: each change has commits of its own on its new parent. -/
  new_ahead : ∀ v p, w.newBase v = some p → ¬ w.reach (w.new v) (w.new p)

/-- A moment during `submit`: whether the (atomic) push has happened, and each
PR's base. -/
structure Moment (V : Type) where
  pushed : Bool
  base : V → Option V

/-- A branch's tip as the remote has it. -/
def tip (pushed : Bool) (v : V) : C := if pushed then w.new v else w.old v

def baseTip (m : Moment V) (v : V) : C :=
  match m.base v with
  | none => w.trunk
  | some p => w.tip m.pushed p

/-- PR `v` counts as merged at moment `m`: its base contains its head. -/
def Merged (m : Moment V) (v : V) : Prop := w.reach (w.tip m.pushed v) (w.baseTip m v)

def moving (v : V) : Prop := w.oldBase v ≠ w.newBase v

/-- The moments the protocol can be at, whatever order the individual
retargets happen in. Before the push, a PR has its old base, or trunk if its
base is changing. After it, a PR has its new base, or trunk if it's still
waiting to be retargeted. (A PR opened after the push gets its new base.) -/
def Visited (m : Moment V) : Prop :=
  if m.pushed then ∀ v, m.base v = w.newBase v ∨ (w.moving v ∧ m.base v = none)
  else ∀ v, m.base v = w.oldBase v ∨ (w.moving v ∧ m.base v = none)

/-- **Every moment of the protocol is safe.** -/
theorem visited_safe (h : w.Assumptions) (m : Moment V) (hm : w.Visited m) (v : V) :
    ¬ w.Merged m v := by
  unfold Visited at hm
  unfold Merged baseTip tip
  cases hp : m.pushed <;> simp only [hp] at hm ⊢
  · rcases hm v with hb | ⟨_, hb⟩ <;> rw [hb]
    · cases hob : w.oldBase v with
      | none => exact h.trunk_old v
      | some p => exact h.old_ahead v p hob
    · exact h.trunk_old v
  · rcases hm v with hb | ⟨_, hb⟩ <;> rw [hb]
    · cases hnb : w.newBase v with
      | none => exact h.trunk_new v
      | some p => exact h.new_ahead v p hnb
    · exact h.trunk_new v

/-! ## The protocol, step by step -/

def setBase [DecidableEq V] (m : Moment V) (v : V) (b : Option V) : Moment V :=
  { m with base := fun u => if u = v then b else m.base u }

/-- One step of `submit`. The push may only happen once every PR whose base is
changing has been moved to trunk; that precondition is the whole point. -/
inductive Step [DecidableEq V] : Moment V → Moment V → Prop
  | toTrunk {m v} : m.pushed = false → w.moving v → Step m (setBase m v none)
  | push {m} : m.pushed = false → (∀ v, w.moving v → m.base v = none) →
      Step m { m with pushed := true }
  | toNew {m v} : m.pushed = true → Step m (setBase m v (w.newBase v))

inductive Reachable [DecidableEq V] : Moment V → Prop
  | start : Reachable ⟨false, w.oldBase⟩
  | step {m m'} : Reachable m → w.Step m m' → Reachable m'

theorem reachable_visited [DecidableEq V] {m : Moment V} (hr : w.Reachable m) : w.Visited m := by
  induction hr with
  | start => simp [Visited]
  | step _ hs ih =>
    cases hs with
    | toTrunk hp hmv =>
      rename_i m v
      simp only [Visited, setBase, hp] at ih ⊢
      intro u
      by_cases huv : u = v
      · subst huv; exact Or.inr ⟨hmv, by simp⟩
      · simp only [huv, ite_false]; exact ih u
    | push hp hall =>
      rename_i m
      simp only [Visited, hp] at ih ⊢
      intro u
      by_cases hmu : w.moving u
      · exact Or.inr ⟨hmu, hall u hmu⟩
      · left
        rcases ih u with hb | ⟨hm, _⟩
        · rw [hb]; exact Classical.not_not.mp hmu
        · exact absurd hm hmu
    | toNew hp =>
      rename_i m v
      simp only [Visited, setBase, hp] at ih ⊢
      intro u
      by_cases huv : u = v
      · subst huv; simp
      · simp only [huv, ite_false]; exact ih u

/-- **`submit` never causes an implicit merge**: no PR is ever merged at any
moment the protocol reaches. -/
theorem submit_safe [DecidableEq V] (h : w.Assumptions) {m : Moment V} (hr : w.Reachable m)
    (v : V) : ¬ w.Merged m v :=
  w.visited_safe h m (w.reachable_visited hr) v

end World

/-! ## Both alternatives fail

Reorder `main ← a ← b` into `main ← b ← a` (`true` is `a`, `false` is `b`).
Commits: 0 trunk, 1 old `a`, 2 old `b` (on 1), 3 new `b` (on 0), 4 new `a` (on 3).
-/

def hist : Nat → List Nat
  | 0 => [0]
  | 1 => [0, 1]
  | 2 => [0, 1, 2]
  | 3 => [0, 3]
  | 4 => [0, 3, 4]
  | _ => []

def reorder : World Bool Nat where
  reach a b := a ∈ hist b
  trunk := 0
  old v := if v then 1 else 2
  new v := if v then 4 else 3
  oldBase v := if v then none else some true
  newBase v := if v then some false else none

theorem reorder_assumptions : reorder.Assumptions where
  trunk_old := by intro v; cases v <;> simp [reorder, hist]
  trunk_new := by intro v; cases v <;> simp [reorder, hist]
  old_ahead := by intro v p h; cases v <;> cases p <;> simp_all [reorder, hist]
  new_ahead := by intro v p h; cases v <;> cases p <;> simp_all [reorder, hist]

/-- Pushing first, with the old bases still in place: `b`'s PR (based on `a`)
is merged, since the new `a` contains the new `b`. This is what happened to
PR #2 on real GitHub. -/
theorem push_first_merges : reorder.Merged ⟨true, reorder.oldBase⟩ false := by
  simp [World.Merged, World.tip, World.baseTip, reorder, hist]

/-- Retargeting straight to the new bases before pushing: `a`'s PR (now based
on `b`) is merged, since the old `b` contains the old `a`. -/
theorem retarget_first_merges : reorder.Merged ⟨false, reorder.newBase⟩ true := by
  simp [World.Merged, World.tip, World.baseTip, reorder, hist]

end GhDags.Retarget
