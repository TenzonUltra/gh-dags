# Proofs

Lean 4 proofs of the two claims gh-dags' correctness rests on. The proofs use
Lean v4.35 with no Mathlib.

```bash
lake build
```

That checks every proof. To see which axioms each main theorem uses:

```bash
lake env lean Axioms.lean
```

Every theorem uses only Lean's standard axioms (`propext`, `Quot.sound`, and,
for `submit_safe`, `Classical.choice`). There is no `sorry`.

## 1. Hoisting is correct (`GhDags/Hoist.lean`)

`land x` rearranges `x`'s root path into one trunk stack, so `x` and what it
needs can merge as a prefix.

**Model.** The physical layout is a parent function `par : N → Option N`, where
`none` means trunk. `Anc par a v` means `a` is a physical ancestor of `v`;
`LAnc deps a v` means `a` is a logical one. The invariant `Inv` says every dep
is a physical ancestor.

**What hoisting does** (`Setup.newPar`). Let `P` be `x`'s root path and
`C = P.filter need ++ P.filter (not need)`.

- Path nodes sit on their predecessor in `C`.
- `x`'s stack successor sits on the last node of `C`.
- Every other branch hanging directly off the path (a *side root*) is
  re-anchored.
- Everything else keeps its parent.

**The re-anchoring rule** is a hypothesis, not a fixed function: the anchor is
a path node at or after (in `C`) everything the branch needs from the path. So
the theorem covers any implementation of the rule. The Rust code's
`max_by_key` is one.

**Theorems**, for any layout satisfying the invariant (`Setup.Hyp`):

| Theorem | Says |
|---|---|
| `newPar_inv` | The new layout satisfies the invariant. |
| `newPar_exact` | `x`'s physical ancestors are exactly its logical ancestors: no false dependencies are left. |
| `newPar_rooted` | Every change still reaches trunk. |
| `newPar_acyclic` | So there are no cycles. Without this, `newPar_inv` would hold trivially for a cyclic layout. |

**How the proof goes.** On a chain, ancestry is exactly "comes before"
(`chain_anc` in `Chain.lean`). Ancestry off the path survives unchanged
(`lift`). Any branch that reaches the path leaves it at a node hanging right
off it (`exit`), and what the branch needs from the path is below wherever it
left (`exit_covered`). The rest is a case split on where a change sits.

## 2. `submit` never triggers an implicit merge (`GhDags/Retarget.lean`)

GitHub marks a PR merged as soon as its base contains its head.

**Model.** Commits are known only through reachability. Each change has an old
tip and a new tip, and an old base and a new base. `Assumptions` holds for any
restacked layout:

- trunk contains no change's work;
- every old head is ahead of its old base;
- every new head is ahead of its new base.

**The protocol** (`Step`):

1. Retarget each PR whose base changes to trunk.
2. Push everything atomically, which is allowed only once every such PR is on
   trunk.
3. Retarget those PRs to their new bases.

The retargets in steps 1 and 3 can happen in any order.

| Theorem | Says |
|---|---|
| `submit_safe` | No PR is merged at any moment the protocol can reach. |
| `push_first_merges` | Pushing before retargeting merges one: on a two-change reorder, `b`'s PR ends up merged into `a`. This is exactly what happened to PR #2 on real GitHub. |
| `retarget_first_merges` | Retargeting straight to the new bases before pushing merges one too. |

## From the model to the code

- **Hoist.** The exhaustive Rust search (`matches_proven_model` in
  `src/layout/exhaustive.rs`) checks every hoist it visits. That's all states
  with up to 4 changes, 8.37 million transitions. On each, the code's result
  must be exactly `newPar`, with an anchor that satisfies the rule. So within
  that bound the theorems hold of the code itself. Beyond it, they hold of the
  algorithm the code implements.
- **Retarget.** `submit` performs steps 3 to 5 of DESIGN.md §6 in this order.
  The assumptions match what `submit` does first: it restacks, refuses PRs that
  have already merged, and pushes with `git push --atomic`. The GitHub fuzz
  checks the same property end to end against a fake GitHub that implements
  implicit merges.

## What formalizing found

- **`submit` could retarget a PR that was still stacked.** I found this while
  writing down the protocol's precondition: "every PR whose base changes is out
  of its stack". The pre-push plan only looked at each stack up to its first
  change without a PR. So linking a new, PR-less change in under a stacked PR
  left that PR stacked, and GitHub refused the retarget with a 422. Now any
  stack holding such a PR is dissolved. A regression test and the GitHub fuzz
  both cover it.
- **`land` over-anchored next to parked changes.** The refinement check found
  this. When a branch off the path had a parked change on it, the code counted
  the parked change's needs as the branch's own, even though it gets some of
  them from its sum's other member. It also let anchors land above `x`. So
  `land a` with nothing to hoist still moved a branch and unparked a change.
  Re-anchoring now only considers the path, as the model requires.

## Not covered

- **Sums.** The hoist model has no sums (parked stacks). Changes sitting on a
  sum are skipped by the refinement check, and covered by testing.
- **Stack folding.** `normalize` folds stacks, but it never changes a parent,
  so the theorems are unaffected; it isn't modeled.
- **Everything else is tested, not proven:** the stack planner, git itself
  (that `git replay` moves exactly `fork..branch`), and GitHub's API (modeled by
  the fake).
- **The code-to-model link** for hoist is checked up to 4 changes, not proven.
