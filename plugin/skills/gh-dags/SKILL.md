---
name: gh-dags
description: Work with gh-dags (`gh dags`), which tracks branches as a dependency DAG, keeps each one rebased on what it depends on, and submits them as GitHub stacked PRs. Use it in a repo where gh-dags is set up (`gh dags view` lists changes), or when the user wants stacked or dependent PRs, wants work split into reviewable pieces, or asks to restack, submit, sync, or land a stack.
---

# gh-dags

gh-dags tracks *changes*: branches, each with the changes it depends on. It
keeps every change rebased onto what it sits on, and turns the layout into
GitHub stacked PRs. Commit to its branches as usual, and let it do the
rebasing, pushing, and PR bookkeeping.

## Look before acting

`gh dags view --json` prints the state: each change's deps and PR number, and
the stacks. It's easier to read reliably than the tree that `gh dags view`
draws. If gh-dags isn't set up in the repo, it says so. Run `gh dags init`
only if the user wants it.

## The loop

1. `gh dags new <name>` creates a change on top of the current one and
   switches to it. `--on a,b` names its deps instead, and `--on main` gives it
   none. `-m "<message>"` commits what's staged.
2. Commit with git as usual. Amending a lower change is fine: the next restack
   carries everything above it.
3. `gh dags submit` restacks if needed, pushes, opens draft PRs, and groups
   them into GitHub stacks. It pushes and opens PRs, so run it only when the
   user wants that.
4. `gh dags sync` after anything merged on GitHub. It absorbs merges,
   restacks, and updates PRs.
5. `gh dags land <change>` moves the change and what it needs to the bottom
   of its stack, then merges them into trunk on GitHub. **Run it only when the
   user asks to land or merge that change.** To only reorder, use
   `gh dags land <change> --local`.

## Leave these to gh-dags

Doing these by hand to a tracked branch works against gh-dags:

- **Rebasing** (`git rebase`, or `git reset` onto another base). gh-dags
  records where each change's own commits start, and a hand rebase leaves that
  stale. Use `gh dags restack`. To change what a change sits on, use
  `gh dags move`, `gh dags link`, or `gh dags unlink`.
- **Pushing** (`git push`). `gh dags submit` pushes with a lease on what it
  last pushed, and a plain push breaks that.
- **Opening or retargeting its PRs** (`gh pr create`, changing a PR's base).
  `gh dags submit` keeps PR bases safe and annotates each PR.
- **Editing gh-dags' block in a PR body**, between `<!-- gh-dags:start -->`
  and `<!-- gh-dags:end -->`. Each submit rewrites it. Text above it is kept,
  so write there.

## Changing the layout

- `gh dags link <change> <dep>` adds a dependency, and `gh dags unlink`
  removes one. `gh dags move <change> --onto <change-or-main>` moves a change
  and everything on it.
- A change with several independent deps is stacked in one line by default.
  The extra order is marked ⚠ in `gh dags view` and isn't a real dependency.
  `--park` instead keeps the change local, on a merge of its deps. A parked
  change gets no PR until all but one of its deps have landed.
- Layout changes restack right away. With uncommitted changes in the current
  worktree they don't, so run `gh dags restack` once it's clean.
- `gh dags track <branch>` adopts an existing branch. If it says the deps are
  ambiguous, pass `--on`.

## When a restack doesn't finish

- **A conflict.** The message names the change, and if it's checked out
  elsewhere, the worktree: `restack stopped on a conflict in "x" (checked out
  in /path)`. Resolve the conflict there, `git add` the files, and run
  `gh dags restack --continue`. `gh dags restack --abort` puts every branch
  back where it was.
- **Skipped changes.** `skipped x (and y on top of it): <reason>` means the
  rest restacked but `x` couldn't move:
  - its worktree has uncommitted changes: commit or stash them there, then
    `gh dags restack`;
  - the merge it's parked on can't be built: run the `gh dags move` the
    message suggests.
- `submit`, `sync`, and `land` stop while a change that gets a PR is skipped.
- `a restack is in progress` means a stopped restack is waiting. Finish it
  with `--continue` or `--abort` before anything else.

## Worktrees and other sessions

- A change can be checked out in a worktree of its own. A restack moves a
  *clean* worktree's branch and updates its files. A dirty one holds back its
  change.
- So a restack you run can change files in a worktree that another person or
  agent is using. Check with the user before restacking changes that someone
  else has checked out.
- `gh dags up`, `down`, `top`, `bottom`, and `switch <change>` switch the
  current worktree. Always give `switch` a name, since without one it waits
  for input. If the change is checked out elsewhere, the command says where,
  and `--print-path` prints the path to `cd` to.
- gh-dags commands run one at a time across all worktrees. A second one waits
  up to 30 seconds, printing `waiting for … to finish`, then gives up. Wait
  and retry, and never delete `.git/gh-dags/lock`.

## Ask the user first

- `gh dags land` without `--local`, unless the user asked to land or merge
  that change. It merges into trunk on GitHub.
- `gh dags submit`, unless the user asked for PRs. It pushes and opens them.
- Deleting branches. After a merge, gh-dags prints `git branch -D` hints;
  don't run them unasked.
- `gh dags restack --abort` when the user has partly resolved a conflict,
  since it discards that work.

[reference.md](reference.md) lists every command and flag.
