# gh-dags

Your branches form a dependency DAG. GitHub Stacks only do straight lines.
`gh dags` keeps the DAG as the source of truth. It arranges the branches so
GitHub can show them as ordinary stacked PRs, writes the real dependencies into
each PR, and keeps everything rebased as you work.

```
$ gh dags new login --on auth,theme
layout: login on theme
layout: theme now on auth (was main) · ordering only
restacked theme, login

$ gh dags
main
├── ○ auth           #12
│   ├── ○ auth-tests #15
│   └── ○ theme      #13  ⚠ ordered after auth (independent)
│       └── ◉ login  #14  ← auth, theme
└── ○ docs           #16
```

- **Fan-out is free.** Two changes on the same base become a stack and a
  *spur*, and are reviewed and landed independently.
- **Fan-in linearizes or parks.** A change that needs two independent changes
  either stacks one on the other (flagged as ordering-only), or, with
  `--park`, sits locally on a merge of both while they're reviewed in parallel.
- **`land` reorders for you.** `gh dags land theme` moves `theme`, and only
  what it needs, to the bottom, then merges once GitHub has seen exactly that.

See [DESIGN.md](DESIGN.md) for the model and the reasoning behind it.

## Try it

You need git 2.44 or later (for `git replay`) and the GitHub CLI.

```bash
cargo build --release
```

Then put `target/release/gh-dags` on your PATH and run it as `gh-dags …`. Or
install it as a `gh` extension so it runs as `gh dags …`:

```bash
mkdir -p ~/.local/share/gh/extensions/gh-dags && ln -sf "$PWD/target/release/gh-dags" ~/.local/share/gh/extensions/gh-dags/gh-dags
```

## Commands

| | |
|---|---|
| `init` | Start tracking in this repo. |
| `new <name> [--on a,b] [--park] [-m msg]` | Create a change on what it depends on. Defaults to the current change. |
| `track <branch> [--on …]` / `untrack` | Adopt an existing branch. Deps are inferred from history. |
| `link` / `unlink <change> <dep>…` | Edit the DAG. |
| `move <change> --onto <target>` | Manual layout. Rejected if it would break a dependency. |
| `view` (default) | The layout as a tree. Add `--json` for scripts. |
| `restack [<change>] [--upstack\|--downstack] [--no-trunk]` | Rebase changes onto their parents: all of them, or just above/below a change. `--continue` / `--abort` after a conflict. |
| `up [n]` / `down [n]` | Move along the layout. `up` follows the stack at a fork and names the spurs it passed. `down` crosses from a spur into its anchor. |
| `top` / `bottom` / `trunk` | Ends of the current stack, or the trunk. |
| `switch [<change>]` | Switch to a change. With no name, pick it from the numbered tree. |
| `completion <fish\|zsh\|bash>` | Tab completion for `gh dags` (see below). |
| `submit [--ready]` | Push, open or update PRs, and make GitHub stacks match. |
| `sync` | Absorb merges, take GitHub's rebases, restack, update. |
| `land <change> [--method squash] [--local]` | Hoist, then merge. |

## Tab completion

`gh` doesn't pass completion through to extensions, so `gh dags` ships its own
hook. It completes subcommands, flags, change names (described by where each
change sits and its PR), a change's deps for `unlink`, untracked branches for
`track`, and comma lists for `--on`.

fish (gh's own fish script resets every `gh` completion when it loads, so both
go in one file; fish loads the first `gh.fish` it finds, so this takes over
from the one Homebrew installs):

```bash
gh dags completion fish > ~/.config/fish/completions/gh.fish
```

zsh: add `eval "$(gh dags completion zsh)"` to `~/.zshrc` after `compinit`.
bash: add `eval "$(gh dags completion bash)"` to `~/.bashrc`.

## Status

The local workflow is covered by unit and end-to-end tests. The GitHub side is
tested against a fake `gh` that enforces the Stacks API rules we rely on. The
whole lifecycle has also run against real GitHub: submit with a stack and a
spur, a reorder via `land`, two squash merges, `sync`, and the spur re-rooting.
Merge queues and required checks are still untested (DESIGN.md §10).

```bash
cargo test
```

[VERIFICATION.md](VERIFICATION.md) has the details: exhaustive checking of the
layout, model-based fuzzing of restack and of the GitHub flow, mutation testing
to show those checks catch bugs, and Lean proofs of the two core claims
([proofs/](proofs/README.md)). The long runs use
`cargo test --release -- --ignored`.
