# gh-dags

Stacked PRs for branches with more than one dependency.

> Built mostly with coding agents, as a personal tool. I'd rather use it on
> projects I care about than polish every line here. If you build it more
> properly, let me know :)

GitHub Stacks only do straight lines. `gh dags` tracks the real dependency
DAG, lays it out as stacks GitHub can show, and keeps it rebased.

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

`login` needs `auth` and `theme`, so `theme` was moved onto `auth` to make one
line. ⚠ marks that order as layout-only.

## Install

Needs git 2.44+ and `gh`.

```bash
cargo build --release
mkdir -p ~/.local/share/gh/extensions/gh-dags
ln -sf "$PWD/target/release/gh-dags" ~/.local/share/gh/extensions/gh-dags/gh-dags
```

## Use

```
gh dags init
gh dags new <name> [--on a,b] [--park]
gh dags submit
gh dags sync
gh dags land <change>
```

Also: `view`, `track`, `untrack`, `link`, `unlink`, `move`, `restack`, `switch`,
`up`, `down`, `top`, `bottom`, `trunk`, `completion <fish|zsh|bash>`,
`import` (rebuild a submitted DAG on another clone), `checkout <pr>`.
With a worktree per change, navigation takes `--print-path`:
`cd "$(gh dags up --print-path)"`.

## More

[DESIGN.md](DESIGN.md) for the model, [VERIFICATION.md](VERIFICATION.md) for
testing. Merge queues and required checks are untested.
