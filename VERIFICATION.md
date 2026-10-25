# Verification

Ordering mistakes can break gh-dags in three places, and each gets its own
check. Every check runs on the real code. Mutation testing (§4) confirms each
one fails when a bug is planted.

| Layer | What could go wrong | How it's checked |
|---|---|---|
| Layout operations | An operation breaks the invariant, does the wrong thing, or refuses something legal | Exhaustive search of every small state |
| Restack | Wrong replay order or fork point, a commit dropped or duplicated, a bad sum, a broken abort, a worktree left behind its branch | Model-based fuzzing against real git |
| GitHub sequencing | Implicit merges, retargeting while stacked, stale stacks, missed merges | Model-based fuzzing against a fake GitHub |

## 1. Exhaustive bounded checking (`src/layout/exhaustive.rs`)

The search starts from an empty state and visits **every distinct state**
reachable within a bound. States are deduplicated up to creation order, so it
covers the space, not just paths through it. From each state it applies every
operation:

- `new` with every subset of deps, both linearized and parked;
- `link` and `unlink` for every pair;
- `move` onto every target;
- `land` (hoist), `absorb` (merge), and `untrack` for every change.

Each transition is then checked against exact properties, not just the
invariant:

- **Rejected operations change nothing.**
- **Legal operations aren't rejected.** For example: `land` of any unparked
  change, absorbing any trunk stack's bottom, or `new` whenever its deps don't
  need fan-in.
- **The result is valid and canonical.** It passes `validate()`, is already
  normalized, and the topological order respects every physical ancestor.
- **`new`:**
  - deps are recorded reduced, and no other change's deps change;
  - no existing change loses a physical ancestor;
  - without fan-in, nothing else moves, and the new change gets no false
    dependencies beyond its parent's;
  - `--park` sits exactly on the sum of the independent deps.
- **`link` / `unlink`:**
  - `link` makes the dep a physical ancestor;
  - `unlink` leaves the layout byte-for-byte unchanged.
- **`move`** puts the change exactly on the target.
- **`land` (hoist):**
  - afterwards, the change's physical ancestors are *exactly* its logical
    ancestors;
  - they form a prefix of a trunk stack;
  - it reports a change exactly when the layout changed;
  - running it again changes nothing.
- **Absorbing a merge:** every other change's ancestry loses exactly the merged
  change, and nothing else.
- **Trees never get false dependencies**, checked separately for up to 5
  changes.
- **`import` gets the layout back.** For every state, project what `submit`
  would show GitHub (PR bases, metadata, stacks), rebuild from it, and require
  the same layout, deps, and creation order. Without GitHub's stacks, require
  the same parents (`src/import.rs`).

| Run | States | Transitions |
|---|---|---|
| default: ≤3 changes, depth 8 | 750 | 13,336 |
| default: ≤4 changes, depth 6 | 4,272 | 35,644 |
| long: ≤4 changes, depth 12 | 290,757 | 8,370,795 |

## 2. Restack against real git (`tests/fuzz.rs`)

Each change owns one file, `<name>.txt`, which holds its current version. A
model records every commit made for each change. Random sequences then run
against a real repository with a bare `origin`:

- new changes (fan-in, parking, spurs);
- amends;
- trunk moving on;
- squash merges plus absorbing;
- `link`, `unlink`, `move` (including move-downs that must drop commits);
- `land` (hoist);
- scoped and full restacks;
- worktrees: checking a change out in one of its own, leaving an uncommitted
  edit there, undoing it, and removing the worktree.

After every full restack, **every branch** must satisfy:

- **Exact contents.** It holds the files of itself, its physical ancestors, and
  everything merged, each at the model's current version, and nothing else.
- **Exact placement.** Its top *k* commits are exactly its own *k*. Directly
  below them is its parent's tip, or for a parked change, a merge commit whose
  parents are the sum's tips in order.
- **A correct fork point.** The recorded fork point is exactly where its
  commits start.
- **Stability.** Restacking again moves nothing.

After `restack --upstack/--downstack`, branches outside the scope must not have
moved, and everything inside it must be placed correctly. It may refuse to put
a change on a parent outside the scope that still has the change's commits.
Then that parent, or something under it, must really be out of place, and
nothing may have moved.

**Worktrees.** After every step, each worktree is still on its change. A clean
one has a clean status, so its files are the new tip's. A dirty one still has
exactly its uncommitted edit. After every gh-dags command, nothing in a dirty
worktree has moved. Each change a restack says it skipped must be in a dirty
worktree and must have needed to move, and nothing stacked on it may have
moved either. Everything else gets the full checks above.

**Conflicts.** A descendant edits a change's file, then the change is amended
so the replay conflicts. Restack must stop on the conflict. After `--abort`,
every branch tip, the state file *byte for byte*, and HEAD are exactly as they
were. After resolving and running `--continue` (possibly through several
conflicts), everything is placed correctly, the resolution sticks, and HEAD is
back where it started. Most changes are checked out in worktrees of their own,
and half the time trunk moves too, so branches move before the conflict stops
the restack. The message has to say which worktree the conflict is in, it's
resolved there, and `--continue` or `--abort` runs from either worktree. After
both, every worktree is on its change with a clean status.

Every seed runs on both rebase paths: `git replay`, and the `git rebase`
fallback (forced with `GH_DAGS_NO_REPLAY`). Both are held to the same model.

**Coverage floors.** Each test counts the hard cases (parked changes checked,
move-downs, reordering hoists, merges, scoped restacks, changes moved under
clean worktrees, changes held back by dirty ones, conflicts in other
worktrees) and fails if too few came up. This matters. The first version of
the fuzz passed while two planted bugs went uncaught, because random choices
almost never built parked changes or legal move-downs (§4).

| Run | Full checks | Scoped | Parked | Move-downs | Hoists | Merges | Moved in worktrees | Held back |
|---|---|---|---|---|---|---|---|---|
| default (10 seeds × 60 steps) | 89 | 37 | 23 | 26 | 22 | 18 | 108 | 34 |
| long (40 seeds × 80 steps) | 455 | 258 | 70 | 139 | 140 | 76 | 452 | 527 |

Conflicts: 12 seeds by default (5 aborted, 5 continued, 5 in other
worktrees); 80 in the long run (32 aborted, 44 continued, 31 in other
worktrees).

## 3. The GitHub flow against a fake GitHub (`tests/fake-gh/gh`)

The fake `gh` keeps PRs and stacks in a JSON file and merges into the bare
`origin`. It enforces every GitHub rule gh-dags relies on. Each rule was
**checked against real GitHub** first (DESIGN.md §10):

- implicit merges (a PR whose base contains its head counts as merged);
- no retargeting while stacked;
- a stack needs at least two PRs, chained base to head;
- no merging drafts, with the async merge API's failure shape;
- a stack merge squashes each PR in a prefix, leaves the upper ones based on
  the PR below, then rebases the rest of the stack onto trunk and retargets it;
- merged PRs report their merge commit.

The fuzz drives `submit`, `land` (real merges), and `sync` through random
layouts, reorders, fan-in across stacks, and growing spurs. After every
`submit` it checks:

- **No PR was ever implicitly merged.**
- **Every PR is right:** each submittable change has an open PR with the right
  head, its physical parent as base, and dependency metadata matching the
  state.
- **The remote is right:** every remote branch equals the local one.
- **The stacks are right:** every layout stack of two or more PRs exists as an
  open GitHub stack with the same PRs, in the same order, on the same base, and
  no stale stacks remain.

After each merge, the full git oracle from §2 runs too.

| Run | Submits | Stack rebuilds | Spur stacks | Merged | Multi-PR merges | PR-less inserts under a stack |
|---|---|---|---|---|---|---|
| default (3 seeds × 6 rounds) | 18 | 4 | 3 | 13 | 6 | 3 |
| long (12 seeds × 12 rounds) | 144 | 43 | 44 | 177 | 71 | 23 |

## 4. Does it catch bugs?

Each mutant is a deliberately planted bug (two of them are real bugs I hit
earlier, put back).

| Planted bug | Caught by | How it showed |
|---|---|---|
| `land` skips re-anchoring spurs | §1, needs 4 changes | `"c" depends on "a", but isn't stacked on it` |
| `land` refuses changes on spurs (the old behavior) | §1 | completeness: a legal hoist was rejected |
| Restack ignores the fork point (the old move-down bug) | §2 | `c3's top commits are ["c0 v2", "c3 v2"]` |
| Restack misses the fork-below-parent case | §2 | a second restack moved something |
| Sum commit parents in the wrong order | §2 | doesn't sit on the sum |
| Sum commits not deterministic | §2 | a second restack moved something |
| Restack doesn't record the new fork point | §2 | restack failed |
| `--abort` doesn't restore fork points | §2 conflicts | the state file isn't restored exactly |
| `--continue` doesn't record the fork point | §2 conflicts, after the fork-point check was added | recorded fork point isn't where its commits start |
| No retarget-to-trunk before pushing (the real-GitHub bug) | §3 | `GitHub implicitly merged [1]` |
| Absorb only PRs based on trunk | §3, after multi-PR merges were added | a merged change still tracked, holding others' files |
| Unstack only what the plan rebuilds | §3, after PR-less inserts under stacks were added | GitHub's 422: a stacked PR's base can't change |
| A worktree's branch moves without the worktree following | §2 | `c1's worktree has status "D  c0.txt"` |
| A dirty worktree's branch is moved anyway | §2 | `c1 moved, though a dirty worktree holds it back` |
| Clean worktrees are treated as dirty | §2 | `c1 was skipped, but no dirty worktree has it` |
| A dirty change is skipped, but not what's stacked on it | §2 | `c3 moved, though a dirty worktree holds it back` |
| A conflict rebase runs at home, not in the change's worktree | §2, rebase path | restack failed: git won't check out a branch another worktree has |
| `--abort` moves refs back without their worktrees following | §2 conflicts, after trunk moves were added | `c0's worktree has status "A  main.txt"` |
| `--continue` resumes the rebase in the wrong worktree | §2 conflicts | `--continue` failed |

Two of these needed the checks themselves fixed first:

- The `--continue` mutant was "self-healing": the next full restack repaired the
  stale fork point. A scoped restack could still have hit it later, so the fork
  point is now checked directly.
- The fuzz caught the trunk-based absorb mutant only once it was steered into
  merging several PRs at once.
- The `--abort` worktree mutant survived until conflict cases also moved
  trunk. Before that, no branch in a worktree had moved by the time the
  conflict stopped the restack, so abort had nothing to put back.

### What this found in the real code

- **A real bug in `sync`.** Landing a change with dependencies below it merges
  several PRs at once. GitHub leaves the upper ones based on the PR below them,
  and `sync` only absorbed PRs based on trunk. So it dropped their PR numbers
  and kept tracking changes that had already landed. A two-PR merge on real
  GitHub confirmed GitHub's behavior. `sync` now decides "landed" by whether
  the merge commit is in trunk.
- **Scoped restacks could drop a change's commits.** Suppose the layout changed
  but wasn't restacked yet, because a worktree was dirty. `restack x --upstack`
  then put `x` on a parent that still had `x`'s commits, and the replay dropped
  them as already applied. The worktree fuzz found it, and it reproduced on
  `main` with a dirty home worktree. A scoped restack now refuses that case.
- **`--park` was silently ignored** when the deps were already in one line. It
  now says so.
- **`sync` said nothing** when a merge unparked a change that has no PR yet. It
  now points at `submit`.

## 5. Proofs (`proofs/`)

Two claims are proven in Lean 4 for layouts of any size, with no `sorry` and
only Lean's standard axioms ([proofs/README.md](proofs/README.md)):

- **Hoisting (`land`) is correct.** For any layout satisfying the invariant,
  and any re-anchoring that follows the rule:
  - the result satisfies the invariant;
  - the change's physical ancestors are exactly its logical ones;
  - every change still reaches trunk, so there are no cycles.
- **`submit` never triggers an implicit merge.** Retargeting to trunk, pushing
  atomically, then retargeting is safe at every intermediate moment. Concrete
  counterexamples show both naive orders aren't. One of them is the real PR #2
  incident.

The proofs are about models, so the exhaustive search (§1) also checks every
hoist it visits against the Lean model: the code's result must be exactly the
model's new layout. On all 8.37 million transitions with up to 4 changes it is,
so the theorems hold of the code there, and of its algorithm beyond.

Formalizing found two more bugs:

- **`submit` could retarget a PR that was still stacked.** I found this while
  stating the protocol's precondition. The pre-push plan stopped at a stack's
  first change without a PR, so a new change linked in under a stacked PR left
  that PR stacked, and GitHub refused the retarget.
- **`land` over-anchored next to parked changes.** The model check found this.
  The code counted a parked change's needs as its branch's own, even though the
  parked change gets some of them from its sum's other member, and let anchors
  land above the change. So `land` with nothing to hoist still moved things and
  unparked a change.

Both are fixed, with regression tests. The fuzz now builds the first shape on
purpose (the mutant above).

## Running it

```bash
cargo test
```

That's the default suite: everything above at the "default" size, about two
minutes. CI runs it on Linux and macOS, and against Debian's git 2.39, which
predates the `git replay` gh-dags uses, so every restack takes the `git rebase`
path there. A release tag also runs it before building.

```bash
cargo test --release -- --ignored
```

That's the long runs, about 11 minutes. `cd proofs && lake build` checks the
proofs. Reproduce one seed with
`FUZZ_SEED=<n>`, print its action log with `FUZZ_LOG=1`, and scale up with
`FUZZ_SEEDS=<count>`.

## Limits

- **Bounded.** The exhaustive search covers up to 4 changes; the fuzz goes up
  to about 8. A bug that needs more changes to show could still hide. The
  "re-anchoring needs 4 changes" mutant shows such bugs exist.
- **The fake GitHub is a model.** It's calibrated against real GitHub for every
  rule listed. Merge queues, required checks, and repos that auto-delete head
  branches aren't modeled or tested.
- **Not covered:** other tools changing branches while gh-dags runs (gh-dags
  commands themselves take a lock, and a test runs them in pairs), and zsh
  completion (syntax-checked only). The GitHub fuzz runs without worktrees, so `sync`'s
  handling of them has end-to-end tests only.
- **Proofs remove the size bound only for the two core claims.** See §5.
  Everything else is tested, not proven.
