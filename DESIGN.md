# gh-dags: dependency DAGs on top of GitHub Stacks

`gh dags` is a `gh` extension. You describe your work as a DAG of changes
("login-ui needs auth and theme"). It keeps the branches physically arranged
so GitHub can show them as ordinary **Stacks**, and it keeps the arrangement
up to date as you edit, reorder, and land.

## 1. Constraints we have to live with

| Layer | What it supports |
|---|---|
| git | A commit can have several parents. A *branch* has one tip, and "based on" means one chain of history. |
| GitHub Stacks | Linear chains only. Each PR's base must equal the head of the PR below it. All branches live in one repo. A merge lands a prefix of the stack. The stack API can only create, append, or dissolve ("unstack"), never reorder. |
| Merging | A PR can merge only if every PR below it meets branch protection too. |

So the DAG cannot reach GitHub as-is. It has to be *projected* onto chains,
and the projection should be as faithful, and as stable, as possible.

## 2. Three layers

```
logical DAG        (you author this)       login-ui ← auth, theme
     │
physical layout    (gh-dags maintains)      main ← auth ← theme ← login-ui
     │
GitHub projection  (gh-dags pushes)         stack #41: #12 ← #13 ← #14
```

**Change.** A branch that becomes one PR. Its name is the branch name.

**Deps.** The logical parents of a change, i.e. the edges of the DAG. Trunk
(`main`) is the implicit root, so a change with no deps depends only on trunk.

**Stack.** An ordered chain of changes. Each change is physically based on the
one below it. The bottom change is based on the stack's **base**, which is one of:

| Base | Name | GitHub projection |
|---|---|---|
| `trunk` | stack | A GitHub Stack targeting `main` |
| another change `a` | **spur** | PRs based on `a`'s branch (a Stack if GitHub accepts the base, plain PRs otherwise; see §10) |
| a *sum* `a ⊕ b ⊕ …` | **parked** | Not submitted. Local only. |

A change's **physical parent** is its predecessor in its stack, or else the
stack's base. Its **physical ancestors** are everything reachable by following
physical parents.

### The invariant

> Every dependency of a change is one of its physical ancestors.

This is the only rule. It guarantees that every branch contains everything it
needs, and that every PR's diff (head vs. its base branch) shows only its own
change. Every layout operation is checked against it before anything is
written to disk.

A physical ancestor that is *not* a logical ancestor is a **false dependency**.
It is the price of linearizing. gh-dags tries to keep false dependencies few,
shows them in `view`, and writes the real deps into each PR's description.

## 3. From DAG to layout

**Fan-out (trees) costs nothing.** If `B` and `C` both depend only on `A`, then
`C` becomes a spur on `A`:

```
DAG                 layout                      GitHub
main ─ A ─ B        main ← A ← B                stack: A ← B
         └ C               └── C  (spur on A)   C's PR targets A's branch
```

`B` and `C` are reviewed and landed independently. Nothing is falsely ordered.

**Fan-in has to pick one of two strategies.** `C` needs both `A` and `B`, which are
independent of each other. One branch cannot have two bases, so either:

1. **Linearize (default).** Stack one side on the other, `main ← A ← B ← C`.
   This gives `B` a false dependency on `A`, but everything is a normal GitHub
   Stack, and `land B` (§4) can reorder later.
2. **Park (`--park`).** Base `C` on a *sum*: a synthetic local merge commit of
   `A` and `B`. This is the "direct sum of two independent things", made
   concrete. `A` and `B` stay in separate stacks and are reviewed in parallel.
   You develop `C` with both present. `C` isn't submitted while parked. Once
   all but one of its sum members has landed, `sync` unparks it onto the
   remaining one automatically. If `A` and `B` conflict textually, the sum
   can't be built, which tells you they aren't really independent.

The sum commit is built without touching the worktree:
`git merge-tree --write-tree A B` followed by `git commit-tree -p A -p B`. Its
author, committer, and date are fixed, so identical tips always produce the
identical commit. That means no caching and no churn.

**Choosing which side moves when linearizing.** Take deps `x` and `y` that are
not on one path, with lowest common physical ancestor `c`. Let `X` and `Y` be
the first nodes after `c` on the way to `x` and `y`. There are two minimal
moves: re-parent `Y` onto `x`, or re-parent `X` onto `y`. Each one is a single
parent-pointer change, and each one only *adds* ancestors, so the invariant
still holds. We pick the move that creates fewer false dependencies (size of
the moved subtree × length of the segment it lands on). Ties go to moving the
newer work on top. You can override this with `move`.

### Stability

A reorder rewrites every branch above the moved point. That means
force-pushes, CI reruns, and possibly stale approvals. So **the layout is
persistent state, not a function recomputed from the DAG.** It changes only
through explicit operations:

| Operation | Layout effect |
|---|---|
| `new X --on D…` | Append to a stack if the deepest dep is a stack top. Otherwise start a spur. For fan-in, linearize or park. |
| `link X D` | Record the edge. If `D` isn't already below `X`, re-parent minimally, as above. |
| `unlink X D` | Record only. The layout stays put, and `D` becomes a false dependency. |
| `move X --onto T` | Re-parent `X`'s subtree onto `T`. Rejected if it would break the invariant. |
| `land X` | Hoist `X` and its logical ancestors to the bottom of `X`'s stack, keeping their relative order (see §4). |
| merged upstream | Remove the change. Its children re-root onto its parent, and sums shrink (possibly unparking). |

After each operation one normalization runs. A spur hanging off the top of a
stack, when it's the only one hanging there, is folded into that stack. This
is the same physical layout with one fewer GitHub stack.

## 4. `land X`

GitHub merges only a prefix of *one* stack. So `land X` works on `X`'s whole
root path, which can run through spurs:

1. Let `path` be the chain from trunk to `X`. Let
   `required = {X} ∪ (logical ancestors of X)`.
2. The path becomes a single trunk stack: `required` (in current order), then
   the rest of the path (in current order), then whatever sat above `X`. Every
   non-required node only gains ancestors. Every required node only loses
   non-ancestors. So the invariant holds.
3. Whatever a stack had beyond the path stays attached to the path node it sat
   on. Spurs anchored on a moved node are re-anchored onto the highest node they
   actually need, so they don't lose a dependency.
4. If the layout changed: restack, push, and rebuild the GitHub stack. Then
   stop, because CI has to run on the new heads. Re-running `land X` is
   idempotent. Once nothing needs to move, it marks the prefix's PRs ready for
   review (GitHub won't merge drafts) and merges the prefix through GitHub's
   stack-merge endpoint. Then it runs `sync`.

This covers two spur cases. If `theme` was linearized onto `auth` only for
ordering, it moves down to trunk and doesn't wait for `auth`. If a spur's
change really needs its anchor, it joins the anchor's stack, and the anchor's
other children become the spur. Nothing moves physically in that case, but the
GitHub stack has to change.

## 5. Restack engine

Each change records its **fork point**: the commit of its physical parent at
the last successful restack. Restacking change `v` onto parent tip `P` is

```
git replay --ref-action=print --onto P  fork(v)..v     # fast path: no checkout
git rebase --onto P fork(v) v                         # slow path: on conflict
```

The fork point is what makes this correct under amends, squash merges, and
re-roots. The replay moves exactly `v`'s own commits, whatever happened to the
parent.

- Changes are processed in physical topological order: bases first, then
  bottom to top. A stack bottom on `trunk` uses `<remote>/<trunk>` when it
  exists, so PR diffs match GitHub.
- **Fast path:** `git replay` computes new commits in memory. Ref updates are
  applied with `git update-ref --stdin`, with old-value checks. The worktree
  isn't touched, except that a checked-out current branch is re-synced at the
  end, and that requires a clean tree.
- **Slow path:** on a conflict we fall back to a real `git rebase` for that one
  branch. A journal (`restack.json`) records the queue and every branch's
  original tip. `restack --continue` resumes. `restack --abort` restores every
  ref.
- Branches checked out in *other* worktrees are refused up front.

## 6. GitHub projection

`submit` skips parked stacks. The order of its steps matters, because of one
GitHub rule: **a PR is marked merged the moment its base branch contains its
head.** Push a reordered stack (`a b` → `b a`) while #b still targets `a`, and
the new `a` contains `b`, so GitHub "merges" #b into `a`. That PR is then gone
for good. We hit this on real GitHub. Trunk never contains unmerged work, so
it's always a safe temporary base:

1. **Plan the stacks.** For each layout stack, compare the open PRs of its
   remote stack with the desired order. If they're equal, keep it. If the
   remote is a prefix, append to it. Otherwise it has to be rebuilt. Only real
   reorders (`land`, `move`, `link`) take this expensive path.
2. **Free the PRs that move.** Unstack the stacks being rebuilt (a stacked PR's
   base can't change), then retarget every PR whose base changes to trunk.
3. **Push** every branch in one `git push --atomic`, with a per-branch
   `--force-with-lease=<branch>:<last pushed sha>`.
4. **PRs.** Open any that are missing, with base = physical parent branch, draft
   by default, and title from the first commit. A change with no commits gets
   no PR, since GitHub refuses one. Then give the moved PRs their real bases.
   Each head is now strictly ahead of its base.
5. **Stacks.** Create or append.
6. **Annotations.** Each PR body gets a managed block between markers that
   gh-dags owns. The rest of the body is left alone.

PRs whose base doesn't change are safe throughout, since their new tip is
their parent's new tip plus their own commits.

   ```md
   <!-- gh-dags:start -->
   **Depends on** #12, #13 · stacked on #13 · *#13 is ordering-only (independent)*
   <!-- gh-dags:meta {"v":1,"change":"login-ui","deps":["auth","theme"]} -->
   <!-- gh-dags:end -->
   ```

   This puts the DAG in front of reviewers, who would otherwise see only the
   stack map. The `meta` line also lets the DAG be reconstructed on another
   machine.

All API calls go through `gh api`, so auth, hosts (GHES), and `{owner}/{repo}`
resolution come from `gh` itself.

## 7. Sync protocol

`sync` does the following:

1. Runs `git fetch --prune`.
2. Reads PR and stack state.
3. Removes the changes that **landed**: their PR's merge commit is reachable
   from the remote trunk. It doesn't look at the PR's base, because a stack
   merge leaves the upper PRs based on the branch below them (§10). A PR merged
   anywhere else, usually an implicit merge, is forgotten so the change gets a
   new one.
4. Restacks.
5. Pushes and updates PRs and stacks. It never opens new PRs; it says when an
   unparked change needs `submit`.

When the bottom of a stack merges, GitHub's automatic rebase can **rewrite
remote branches**. For each change we track `pushed` (the last sha we pushed):

| local vs `pushed` | remote vs `pushed` | Action |
|---|---|---|
| same | same | Nothing. |
| changed | same | Normal case. Push. |
| same | changed | Remote moved (GitHub rebase or a collaborator). Adopt the remote tip and recompute the fork point. |
| changed | changed | Diverged. Stop and ask. |

## 8. UX

```
$ gh dags
main
├─ auth             #12  approved
│  ├─ theme         #13  review    ⚠ ordered after auth (independent)
│  │  └─ login-ui   #14  draft     ← auth, theme
│  └─ auth-tests    #15  review    spur
└─ docs             #16  draft
   ⊕ search         local          parked on docs ⊕ theme
```

This is the physical layout rendered as a tree from trunk outward. A
continuation in the same stack is the first child, and spurs branch off. Each
line shows the logical deps when they say more than the parent does, false
dependencies are flagged, and parked changes are marked `⊕`. `--json` gives
the full state for scripts.

| Command | What it does |
|---|---|
| `init [--trunk main] [--remote origin]` | Start tracking in this repo. Enables `rerere`. |
| `new <name> [--on a,b] [--park] [-m msg]` | Create and check out a change. Defaults to `--on <current branch>`. |
| `track <branch> [--on a,b]` / `untrack <change>` | Adopt an existing branch, or forget one. |
| `link <change> <dep>…` / `unlink …` | Edit DAG edges. |
| `move <change> --onto <target>` | Manual layout control. Validated against the invariant. |
| `view` (default) | Tree view, as above. |
| `restack [<change>] [--upstack\|--downstack] [--no-trunk]` | Rebase every change onto its physical parent, or only a change's subtree (`--upstack`) or ancestors (`--downstack`). `--no-trunk` leaves trunk-based changes in place. Also `--continue` / `--abort`. |
| `up/down [n]`, `top`, `bottom`, `trunk`, `switch [<change>]` | Navigation (see below). |
| `completion <shell>` | Tab-completion hook (see below). |
| `submit [--ready]` | Push, create or update PRs, update stacks and annotations. |
| `sync` | Absorb merges, restack, update GitHub. |
| `land <change>` | Hoist, then merge the prefix when it's ready. |

**Navigation** follows the physical forest, not just one stack:

- `up` takes the stack successor. At a fork it names the spurs it passed. With
  no successor and several spurs, it stops and lists them.
- `down` follows the physical parent, so it crosses from a spur into its
  anchor. A parked change is ambiguous, because it sits on a sum.
- `top` and `bottom` stay within the current stack, which is the unit GitHub
  shows.
- `switch` with no name numbers the tree's rows and reads a choice.

**Completion.** `gh` registers extensions with no completion hook, so
`gh dags completion <shell>` prints a snippet that takes over when the second
word is `dags`. It calls the hidden `gh dags __complete <words…>`, which
answers from local state only (about 50 ms through `gh`). gh's fish script
erases every `gh` rule when it loads, so for fish the snippet ships *with*
gh's script, as fish's `gh.fish`.

Every mutating command prints a one-line layout diff when the layout moves, so
nothing is reordered silently. For example: `layout: theme now sits on auth
(ordering only)`.

## 9. Storage and architecture

State lives in `<git-common-dir>/gh-dags/state.json`, so it isn't committed
and is shared across worktrees. It is written atomically (temp file plus
rename). `restack.json` is the in-progress journal.

```
src/
  model.rs     State, Change, Stack, Base; graph queries and validate()
  layout.rs    the operations in §3/§4 (pure; every op ends in validate())
  git.rs       thin wrapper over the git CLI
  restack.rs   engine and journal (§5)
  github.rs    `gh api` client
  submit.rs    submit / sync / land (§6–7); the stack planner is pure
  render.rs    tree view
  cli.rs       clap commands
tests/
  cli.rs       end-to-end: the binary against throwaway repos
  fake-gh/gh   a stand-in `gh` (PRs and stacks in a JSON file)
```

Dependencies: `clap`, `serde`/`serde_json`, `anyhow`, and `tempfile` for tests.
How the code is verified (exhaustive checking of the layout, model-based
fuzzing of restack and of the GitHub flow, and mutation testing of both) is in
[VERIFICATION.md](VERIFICATION.md).

## 10. GitHub facts we rely on

Checked against real GitHub (Oct 2026, a private sandbox repo, default API
version):

- **A stack's base can be another stacked PR's head branch.** So spurs show up
  as real Stacks. When the anchor merges, `sync` dissolves the spur's stack,
  retargets its bottom PR to trunk, and recreates it.
- **A stacked PR's base can't be changed** ("Cannot change the base branch
  because the pull request is part of a stack"). Unstack first.
- **A stack needs at least two PRs** (from the `gh stack` source). A one-change
  stack stays a plain PR.
- **Implicit merges.** A PR whose base comes to contain its head is marked
  merged. This drives the order of steps in §6. `sync` forgets a PR that was
  marked merged into a non-trunk branch, so the next `submit` opens a new one.
- **Stack numbers share the issue/PR number sequence.**
- **`PUT /pulls/{n}/merge-async`** (used by `gh stack merge`, not in the public
  docs) merges the stack up to `n` with the given `merge_method`. It refuses
  drafts with HTTP 400 and `{"status":"failed","details":{"message":…}}`, so
  `land` marks the PRs it merges ready for review first. That needs GraphQL,
  because REST can't un-draft a PR.
- **After a merge, GitHub rebases the rest of the stack.** It force-pushes the
  remaining branches and retargets the next PR to trunk
  (`AutomaticBaseChangeSucceededEvent`). `sync` adopts those rewrites (§7). The
  stack keeps its number, with the merged PRs still listed at the bottom.
  Stacks based on a merged branch (spurs) are *not* rebased; `sync` does that.
- **Merging several PRs at once** (a stack prefix) squashes each into trunk in
  order. The upper PRs end `MERGED` but **still based on the PR below them**,
  not on trunk. So "landed" has to be judged by the merge commit, not the base.
  The fuzz found this bug, and a real two-PR merge confirmed the behavior.

Still open:

1. Merge queues and required checks. The sandbox has neither, so the
   `enqueued` and `pending` paths of `land` haven't run for real.
2. Repos with "automatically delete head branches" on. GitHub may then retarget
   spur PRs itself. `sync` should cope, but it's untested.
3. Command name. The repo `gh-dags` installs as `gh dags`. Naming the repo
   `gh-dag` would give `gh dag`.

## 11. Milestones

1. **Local core.** Model, layout ops, restack (replay and rebase,
   continue/abort), `init/new/track/link/unlink/move/view/restack`, park.
2. **GitHub.** `submit`, `sync`, annotations, `land`.
3. **Nice to have.** `up/down/checkout`, `gh dags import <pr>` (rebuild the DAG
   from annotations), interactive layout editor, `plan` (dry-run reorders with
   `git merge-tree` conflict prediction).
