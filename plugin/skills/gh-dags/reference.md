# gh-dags commands

Run `gh dags <command> --help` for the full text of any of these.

## Setting up

| Command | What it does |
|---|---|
| `gh dags init [--trunk <branch>] [--remote <name>]` | Start tracking changes in this repository |
| `gh dags import [<pr>…] [--all]` | Rebuild a DAG that gh-dags submitted (from another clone or machine) from its PRs |
| `gh dags checkout <pr>` | Switch to a PR's change, importing its DAG first if needed |
| `gh dags track <branch> [--on a,b] [--park]` | Adopt an existing branch as a change |
| `gh dags untrack <change>` | Stop tracking a change; the branch stays |

## Making and reshaping changes

| Command | What it does |
|---|---|
| `gh dags new <name> [--on a,b] [--park] [-m <msg>] [--no-checkout]` | New change on its deps (default: the current change) |
| `gh dags link <change> <dep>…` | Add dependencies |
| `gh dags unlink <change> <dep>…` | Remove dependencies; the layout stays put |
| `gh dags move <change> --onto <change-or-trunk>` | Move a change and everything on it |
| `gh dags restack [<change>] [--upstack \| --downstack] [--no-trunk]` | Rebase changes onto their parents, all of them unless scoped |
| `gh dags restack --continue` / `gh dags restack --abort` | Finish or undo a restack stopped on a conflict |

## Looking and moving around

| Command | What it does |
|---|---|
| `gh dags view [--json]` | The layout, or the full state as JSON (the default command) |
| `gh dags up [<n>]`, `gh dags down [<n>]` | The change above or below this one |
| `gh dags top`, `gh dags bottom`, `gh dags trunk` | The top or bottom of this stack, or the trunk |
| `gh dags switch <change>` | Switch to a change by name |

Every navigation command takes `--print-path`: it prints the path of the
worktree that has the change (another one's, or this one's after switching),
and nothing else on stdout.

## GitHub

| Command | What it does |
|---|---|
| `gh dags submit [--ready]` | Push, open PRs (drafts unless `--ready`), and match GitHub's stacks to the layout |
| `gh dags sync` | Absorb merges, take GitHub's rebases, restack, and update PRs |
| `gh dags land <change> [--method squash\|merge\|rebase] [--local]` | Move a change and what it needs to the bottom, then merge them. If that means reordering or pushing first, it does that and asks for a second run once CI is green. `--local` only reorders |

## Messages worth knowing

- `isn't tracked; pass --on, or gh dags track <branch> first`: the current
  branch isn't a change.
- `a restack is in progress`: run `gh dags restack --continue` or
  `gh dags restack --abort`.
- `can't go on … yet: it still has …'s commits`: a scoped restack would drop
  commits. Use the `gh dags restack <change> --downstack` it suggests.
- `kept … local: it's parked on …`: parked changes get no PR yet.
- `… changed both here and on GitHub`: the branch diverged. Reconcile it by
  hand, then run `gh dags submit`.
