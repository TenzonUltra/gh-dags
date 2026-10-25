use std::io::IsTerminal;

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};

use crate::complete;
use crate::github::Gh;
use crate::import;
use crate::layout::{self, FanIn};
use crate::model::{Base, State};
use crate::nav::{self, Nav};
use crate::render::{Style, render, rows};
use crate::restack::{self, Outcome, Scope};
use crate::store::Repo;
use crate::submit;
use crate::worktree;

/// The clap command tree, for completion.
pub fn command() -> clap::Command {
    Cli::command()
}

#[derive(Parser)]
#[command(
    name = "gh-dags",
    version,
    about = "Dependency DAGs of branches, projected onto GitHub stacked PRs"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Where a navigation command lands.
#[derive(clap::Args)]
struct Dest {
    /// Print the path of the worktree that has the change, for `cd`: another
    /// worktree's if it's checked out there, else this one's after switching
    #[arg(long)]
    print_path: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start tracking changes in this repository
    Init {
        /// The branch stacks land on (default: the remote's default branch)
        #[arg(long)]
        trunk: Option<String>,
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Create a change (a new branch) on top of what it depends on
    New {
        name: String,
        /// Dependencies, comma-separated (default: the current change)
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        on: Option<Vec<String>>,
        /// With several independent deps, sit on a local merge of them
        /// instead of stacking them on each other
        #[arg(long)]
        park: bool,
        /// Commit the staged changes to the new branch with this message
        #[arg(short, long, conflicts_with = "no_checkout")]
        message: Option<String>,
        /// Create the branch without switching to it
        #[arg(long)]
        no_checkout: bool,
    },
    /// Track an existing branch as a change
    Track {
        branch: String,
        /// Dependencies, comma-separated (default: inferred from history)
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        on: Option<Vec<String>>,
        #[arg(long)]
        park: bool,
    },
    /// Stop tracking a change (the branch is kept)
    Untrack { change: String },
    /// Record that a change depends on others
    Link {
        change: String,
        #[arg(required = true)]
        deps: Vec<String>,
    },
    /// Remove dependencies (the layout stays put)
    Unlink {
        change: String,
        #[arg(required = true)]
        deps: Vec<String>,
    },
    /// Put a change, and everything on it, onto another change or the trunk
    Move {
        change: String,
        #[arg(long)]
        onto: String,
    },
    /// Show the layout (the default command)
    View {
        #[arg(long)]
        json: bool,
    },
    /// Rebase changes onto their parents (all of them, unless scoped)
    #[command(alias = "rebase")]
    Restack {
        /// The change --upstack/--downstack are relative to (default: the current one)
        change: Option<String>,
        /// Only the change and everything stacked on it
        #[arg(long, conflicts_with = "downstack")]
        upstack: bool,
        /// Only the change and everything it's stacked on
        #[arg(long)]
        downstack: bool,
        /// Leave changes that sit on the trunk where they are
        #[arg(long)]
        no_trunk: bool,
        /// Resume after resolving a conflict
        #[arg(long = "continue", conflicts_with_all = ["abort", "upstack", "downstack", "no_trunk", "change"])]
        cont: bool,
        /// Undo the restack in progress
        #[arg(long, conflicts_with_all = ["upstack", "downstack", "no_trunk", "change"])]
        abort: bool,
    },
    /// Switch to the change above this one (following the stack at a fork)
    Up {
        #[arg(default_value_t = 1)]
        n: usize,
        #[command(flatten)]
        dest: Dest,
    },
    /// Switch to the change this one sits on
    Down {
        #[arg(default_value_t = 1)]
        n: usize,
        #[command(flatten)]
        dest: Dest,
    },
    /// Switch to the top of the current stack
    Top {
        #[command(flatten)]
        dest: Dest,
    },
    /// Switch to the bottom of the current stack
    Bottom {
        #[command(flatten)]
        dest: Dest,
    },
    /// Switch to the trunk branch
    Trunk {
        #[command(flatten)]
        dest: Dest,
    },
    /// Switch to the change for a PR, importing its DAG first if it isn't here
    Checkout {
        pr: u64,
        #[command(flatten)]
        dest: Dest,
    },
    /// Switch to a change (with no name: pick from the tree)
    Switch {
        change: Option<String>,
        #[command(flatten)]
        dest: Dest,
    },
    /// Print the shell code for tab completion of `gh dags`
    Completion { shell: complete::Shell },
    /// Completion candidates for a partial command line (used by `completion`)
    #[command(name = "__complete", hide = true)]
    Complete {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// Push changes and make PRs and GitHub stacks match the layout
    Submit {
        /// Open new PRs ready for review instead of as drafts
        #[arg(long)]
        ready: bool,
    },
    /// Bring a DAG that gh-dags submitted (from anywhere) into this repository
    Import {
        /// PR numbers; what they depend on and sit on comes along
        prs: Vec<u64>,
        /// Every open PR that gh-dags made
        #[arg(long, conflicts_with = "prs")]
        all: bool,
    },
    /// Absorb merges from GitHub, restack, and update PRs and stacks
    Sync,
    /// Move a change and what it needs to the bottom of its stack, then merge
    /// them once GitHub has seen exactly that
    Land {
        change: String,
        /// squash, merge, or rebase (default: the repository's default)
        #[arg(long)]
        method: Option<String>,
        /// Only reorder and restack locally; don't push or merge
        #[arg(long)]
        local: bool,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    // Completion works anywhere, and never fails loudly.
    match &cli.cmd {
        Some(Cmd::Complete { words }) => {
            let repo = Repo::open(std::env::current_dir()?).ok();
            let ctx = complete::Context::load(repo.as_ref());
            for (value, desc) in complete::candidates(&command(), &ctx, words) {
                println!("{value}\t{desc}");
            }
            return Ok(());
        }
        Some(Cmd::Completion { shell }) => {
            let gh_script = match shell {
                complete::Shell::Fish => {
                    let out = std::process::Command::new("gh")
                        .args(["completion", "-s", "fish"])
                        .output()
                        .context("running `gh completion`")?;
                    String::from_utf8_lossy(&out.stdout).into_owned()
                }
                _ => String::new(),
            };
            print!("{}", complete::script(*shell, &gh_script));
            return Ok(());
        }
        _ => {}
    }
    let repo = Repo::open(std::env::current_dir()?)?;
    let style =
        Style::new(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());
    match cli.cmd.unwrap_or(Cmd::View { json: false }) {
        Cmd::Init { trunk, remote } => init(&repo, trunk, remote),
        Cmd::View { json } => {
            let st = repo.load()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                let current = repo.git.current_branch();
                print!("{}", render(&st, current.as_deref(), &style));
                if repo.has_journal() {
                    println!(
                        "{}",
                        style.yellow(
                            "restack in progress: `gh dags restack --continue` or `--abort`"
                        )
                    );
                }
            }
            Ok(())
        }
        Cmd::Restack {
            change,
            upstack,
            downstack,
            no_trunk,
            cont,
            abort,
        } => {
            let mut st = repo.load()?;
            if abort {
                restack::abort(&repo, &mut st)?;
                println!("restack aborted; every branch is back where it was");
                return Ok(());
            }
            if cont {
                return report(restack::resume(&repo, &mut st)?);
            }
            let only = if upstack || downstack {
                let c = match change {
                    Some(c) => c,
                    None => current_change(&repo, &st)?,
                };
                st.change(&c)?;
                Some(if upstack {
                    st.subtree(&c)
                } else {
                    let mut set: BTreeSet<String> = st.phys_ancestors(&c);
                    set.insert(c);
                    set
                })
            } else if let Some(c) = change {
                bail!("pass --upstack or --downstack to restack around {c:?}");
            } else {
                None
            };
            report(restack::start(&repo, &mut st, &Scope { only, no_trunk })?)
        }
        Cmd::Up { n, dest } => {
            let st = repo.load()?;
            let from = here(&repo, &st)?;
            go(&repo, &st, nav::up(&st, from.as_deref(), n)?, dest)
        }
        Cmd::Down { n, dest } => {
            let st = repo.load()?;
            let from = current_change(&repo, &st)?;
            go(&repo, &st, nav::down(&st, &from, n)?, dest)
        }
        Cmd::Top { dest } => {
            let st = repo.load()?;
            let from = here(&repo, &st)?;
            go(&repo, &st, nav::top(&st, from.as_deref())?, dest)
        }
        Cmd::Bottom { dest } => {
            let st = repo.load()?;
            let from = here(&repo, &st)?;
            go(&repo, &st, nav::bottom(&st, from.as_deref())?, dest)
        }
        Cmd::Trunk { dest } => {
            let st = repo.load()?;
            go(
                &repo,
                &st,
                Nav {
                    target: None,
                    note: None,
                },
                dest,
            )
        }
        Cmd::Switch { change, dest } => {
            let st = repo.load()?;
            let target = match change {
                Some(c) => c,
                None => pick(&repo, &st, &style)?,
            };
            if target != st.trunk {
                st.change(&target)?;
            }
            let target = (target != st.trunk).then_some(target);
            go(&repo, &st, Nav { target, note: None }, dest)
        }
        Cmd::Submit { ready } => with_github(&repo, |st, gh| {
            submit::submit(
                &repo,
                st,
                gh,
                submit::Opts {
                    ready,
                    create: true,
                },
                &style,
            )
        }),
        Cmd::Sync => with_github(&repo, |st, gh| submit::sync(&repo, st, gh, &style)),
        Cmd::Checkout { pr, dest } => with_github(&repo, |st, gh| {
            let tracked = |st: &State| {
                st.changes
                    .iter()
                    .find(|(_, c)| c.pr == Some(pr))
                    .map(|(n, _)| n.clone())
            };
            let name = match tracked(st) {
                Some(n) => n,
                None => {
                    let names = import::import(&repo, st, gh, &[pr], false)?;
                    repo.save(st)?;
                    println!("imported {}", names.join(", "));
                    tracked(st).context("the import didn't bring that PR's change")?
                }
            };
            go(
                &repo,
                st,
                Nav {
                    target: Some(name),
                    note: None,
                },
                dest,
            )
        }),
        Cmd::Import { prs, all } => with_github(&repo, |st, gh| {
            if prs.is_empty() && !all {
                bail!("name the PRs to import, or pass --all");
            }
            let names = import::import(&repo, st, gh, &prs, all)?;
            repo.save(st)?;
            println!("imported {}", names.join(", "));
            print!(
                "{}",
                render(st, repo.git.current_branch().as_deref(), &style)
            );
            Ok(())
        }),
        Cmd::Land {
            change,
            method,
            local: false,
        } => with_github(&repo, |st, gh| {
            submit::land(&repo, st, gh, &change, method.as_deref(), &style)
        }),
        cmd => {
            if repo.has_journal() {
                bail!(
                    "a restack is in progress; run `gh dags restack --continue` or `--abort` first"
                );
            }
            let mut st = repo.load()?;
            let before = st.clone();
            mutate(&repo, &mut st, cmd)?;
            settle(&repo, &before, &mut st, &style)
        }
    }
}

/// The tracked change that's checked out.
fn current_change(repo: &Repo, st: &State) -> Result<String> {
    match repo.git.current_branch() {
        Some(b) if st.has(&b) => Ok(b),
        Some(b) => bail!("{b:?} isn't a tracked change"),
        None => bail!("HEAD is detached; switch to a change first"),
    }
}

/// Where navigation starts: a tracked change, or `None` on the trunk.
fn here(repo: &Repo, st: &State) -> Result<Option<String>> {
    match repo.git.current_branch() {
        Some(b) if b == st.trunk => Ok(None),
        _ => current_change(repo, st).map(Some),
    }
}

fn go(repo: &Repo, st: &State, nav: Nav, dest: Dest) -> Result<()> {
    let branch = nav.target.unwrap_or_else(|| st.trunk.clone());
    let here = repo.git.toplevel()?;
    // With --print-path, stdout is just the path, for `cd`.
    let say = |line: &str| {
        if dest.print_path {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    let path = match worktree::checkout_of(&repo.git, &branch)? {
        Some(there) if !worktree::same_path(&there, &here) => {
            if !dest.print_path {
                bail!(
                    "{branch} is checked out in {}; `cd` there to work on it",
                    there.display()
                );
            }
            say(&format!("{branch} is checked out in {}", there.display()));
            there
        }
        _ => {
            if repo.git.current_branch().as_deref() != Some(branch.as_str()) {
                let out = repo.git.exec(&["switch", "--quiet", &branch], None, &[])?;
                if !out.ok {
                    bail!("couldn't switch to {branch}: {}", out.stderr);
                }
            }
            say(&format!("on {branch}"));
            here
        }
    };
    if let Some(note) = nav.note {
        say(&note);
    }
    if dest.print_path {
        println!("{}", path.display());
    }
    Ok(())
}

/// `switch` with no name: number the tree's rows and read a choice.
fn pick(repo: &Repo, st: &State, style: &Style) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!("name the change to switch to");
    }
    let current = repo.git.current_branch();
    let mut choices = Vec::new();
    for (name, text) in rows(st, current.as_deref(), style) {
        match name {
            Some(n) => {
                choices.push(n);
                println!("{} {text}", style.dim(&format!("{:>3}", choices.len())));
            }
            None => println!("    {text}"),
        }
    }
    eprint!("switch to (number or name): ");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    if let Ok(i) = answer.parse::<usize>()
        && (1..=choices.len()).contains(&i)
    {
        return Ok(choices[i - 1].clone());
    }
    if choices.iter().any(|c| c == answer) {
        return Ok(answer.to_string());
    }
    bail!("no change {answer:?}");
}

fn with_github(repo: &Repo, f: impl FnOnce(&mut State, &Gh) -> Result<()>) -> Result<()> {
    if repo.has_journal() {
        bail!("a restack is in progress; run `gh dags restack --continue` or `--abort` first");
    }
    let mut st = repo.load()?;
    let gh = Gh::new(std::env::current_dir()?)?;
    f(&mut st, &gh)
}

fn init(repo: &Repo, trunk: Option<String>, remote: String) -> Result<()> {
    if repo.is_initialized() {
        println!("gh-dags is already set up here");
        return Ok(());
    }
    let git = &repo.git;
    let trunk = match trunk {
        Some(t) => t,
        None => git
            .run(&[
                "symbolic-ref",
                "--short",
                &format!("refs/remotes/{remote}/HEAD"),
            ])
            .ok()
            .and_then(|r| r.split_once('/').map(|(_, b)| b.to_string()))
            .or_else(|| {
                ["main", "master"]
                    .into_iter()
                    .find(|b| git.branch_exists(b))
                    .map(String::from)
            })
            .context("couldn't tell which branch is the trunk; pass --trunk")?,
    };
    if git.resolve(&trunk).is_none() && git.resolve(&format!("{remote}/{trunk}")).is_none() {
        bail!("trunk branch {trunk:?} doesn't exist");
    }
    // Conflicts during restacks repeat; let git remember the resolutions.
    git.run(&["config", "rerere.enabled", "true"])?;
    repo.save(&State::new(&trunk, &remote))?;
    println!("tracking changes on top of {trunk}");
    Ok(())
}

/// Apply a layout command. Branch creation happens here too, since a new
/// change needs its branch before the restack that follows.
fn mutate(repo: &Repo, st: &mut State, cmd: Cmd) -> Result<()> {
    let git = &repo.git;
    match cmd {
        Cmd::New {
            name,
            on,
            park,
            message,
            no_checkout,
        } => {
            if git.branch_exists(&name) {
                bail!("branch {name:?} already exists; use `gh dags track {name}`");
            }
            let deps = match on {
                Some(on) => without_trunk(st, on),
                None => match git.current_branch() {
                    Some(b) if st.has(&b) => vec![b],
                    Some(b) if b == st.trunk => vec![],
                    None => vec![],
                    Some(b) => {
                        bail!("{b:?} isn't tracked; pass --on, or `gh dags track {b}` first")
                    }
                },
            };
            let mut trial = st.clone();
            layout::add_change(&mut trial, &name, &deps, fan_in(park))?;
            if park && !trial.is_parked(&name) {
                println!("note: {name}'s deps are already in one line, so there's nothing to park");
            }
            if others_moved(st, &trial) && !git.is_clean()? {
                bail!(
                    "this reorders existing changes, which needs a clean worktree; commit or stash first"
                );
            }
            *st = trial;
            let start = restack::parent_tip(git, st, &name)?;
            let back = git.head()?;
            if no_checkout {
                git.run(&["branch", "--no-track", &name, &start])?;
            } else {
                git.run(&["checkout", "--quiet", "--no-track", "-b", &name, &start])?;
            }
            st.change_mut(&name)?.fork = Some(start);
            if let Some(msg) = message
                && let Err(e) = git.run(&["commit", "--quiet", "-m", &msg])
            {
                // Leave nothing behind (the state isn't saved either), so
                // the same `new` can just be run again.
                git.run(&["checkout", "--quiet", &back])?;
                git.run(&["branch", "-D", &name])?;
                return Err(e.context(format!("{name} wasn't created")));
            }
        }
        Cmd::Track { branch, on, park } => {
            if !git.branch_exists(&branch) {
                bail!("branch {branch:?} doesn't exist");
            }
            let deps = match on {
                Some(on) => without_trunk(st, on),
                None => infer_deps(repo, st, &branch)?,
            };
            layout::add_change(st, &branch, &deps, fan_in(park))?;
            // Record where the branch's own commits start, so a later restack
            // replays only those.
            let onto = restack::parent_tip(git, st, &branch)?;
            let tip = git.branch_tip(&branch)?;
            let fork = if git.is_ancestor(&onto, &tip) {
                Some(onto)
            } else {
                git.merge_base(&tip, &onto)
            };
            st.change_mut(&branch)?.fork = fork;
        }
        Cmd::Untrack { change } => layout::untrack(st, &change)?,
        Cmd::Link { change, deps } => {
            for d in deps {
                layout::link(st, &change, &d)?;
            }
        }
        Cmd::Unlink { change, deps } => {
            for d in deps {
                layout::unlink(st, &change, &d)?;
            }
        }
        Cmd::Move { change, onto } => {
            let onto = if onto == st.trunk {
                Base::Trunk
            } else {
                Base::Change(onto)
            };
            layout::move_onto(st, &change, onto)?;
        }
        Cmd::Land { change, .. } => {
            if !layout::hoist(st, &change)? {
                println!("{change} is ready: only what it needs is below it");
            }
        }
        Cmd::Init { .. }
        | Cmd::View { .. }
        | Cmd::Restack { .. }
        | Cmd::Submit { .. }
        | Cmd::Sync
        | Cmd::Up { .. }
        | Cmd::Down { .. }
        | Cmd::Top { .. }
        | Cmd::Bottom { .. }
        | Cmd::Trunk { .. }
        | Cmd::Switch { .. }
        | Cmd::Completion { .. }
        | Cmd::Complete { .. }
        | Cmd::Import { .. }
        | Cmd::Checkout { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

/// Save, print what moved, and restack if existing changes moved.
fn settle(repo: &Repo, before: &State, st: &mut State, style: &Style) -> Result<()> {
    repo.save(st)?;
    let moves = layout::describe_moves(before, st);
    for m in &moves {
        println!("{} {m}", style.dim("layout:"));
    }
    if !others_moved(before, st) {
        return Ok(());
    }
    if !repo.git.is_clean()? {
        println!(
            "uncommitted changes, so branches weren't moved yet; run `gh dags restack` when ready"
        );
        return Ok(());
    }
    report(restack::start(repo, st, &restack::Scope::default())?)
}

fn report(outcome: Outcome) -> Result<()> {
    match outcome {
        Outcome::Done(r) => {
            if !r.moved.is_empty() {
                println!("restacked {}", r.moved.join(", "));
            } else if r.blocked.is_empty() {
                println!("everything is already in place");
            }
            for b in &r.blocked {
                println!("{}", b.describe());
            }
            if !r.blocked.is_empty() {
                println!("then run `gh dags restack` again");
            }
            Ok(())
        }
        Outcome::Conflict { change, worktree } => bail!(
            "restack stopped on a conflict in {}. Resolve it, `git add` the files, then run \
             `gh dags restack --continue` (or `--abort` to undo everything)",
            restack::conflict_place(&change, &worktree)
        ),
    }
}

/// Whether any change that existed before now has a different parent.
fn others_moved(before: &State, after: &State) -> bool {
    before
        .changes
        .keys()
        .any(|c| after.has(c) && before.parent(c) != after.parent(c))
}

fn fan_in(park: bool) -> FanIn {
    if park { FanIn::Park } else { FanIn::Linearize }
}

fn without_trunk(st: &State, deps: Vec<String>) -> Vec<String> {
    deps.into_iter()
        .filter(|d| *d != st.trunk && !d.is_empty())
        .collect()
}

/// The tracked changes this branch was built on: those whose tips are in its
/// history, minus any implied by another.
fn infer_deps(repo: &Repo, st: &State, branch: &str) -> Result<Vec<String>> {
    let git = &repo.git;
    let tip = git.branch_tip(branch)?;
    // Tracked changes in the branch's history. One at the branch's own tip
    // counts too: the branch just has no commits of its own yet.
    let mut found = Vec::new();
    for name in st.changes.keys() {
        let t = git.branch_tip(name)?;
        if git.is_ancestor(&t, &tip) {
            found.push((name.clone(), t));
        }
    }
    // The newest of them: none below another one's tip.
    let newest: Vec<(String, String)> = found
        .iter()
        .filter(|(_, t)| !found.iter().any(|(_, u)| u != t && git.is_ancestor(t, u)))
        .cloned()
        .collect();
    // Changes on the same commit: take the one stacked highest. If the
    // layout doesn't order them, history can't say which the branch is on.
    let deps: Vec<&(String, String)> = newest
        .iter()
        .filter(|(n, t)| {
            !newest
                .iter()
                .any(|(m, u)| u == t && m != n && st.phys_ancestors(m).contains(n))
        })
        .collect();
    for (n, t) in &deps {
        if let Some((m, _)) = deps.iter().find(|(m, u)| u == t && m != n) {
            bail!(
                "{n} and {m} point at the same commit, so {branch} could be on either; pass --on"
            );
        }
    }
    Ok(deps.into_iter().map(|(n, _)| n.clone()).collect())
}
