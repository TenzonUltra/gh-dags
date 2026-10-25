//! The restack engine (DESIGN.md §5): rebase every change onto its physical
//! parent, replaying exactly `fork..branch`. The fast path is `git replay`
//! (in memory, no checkout). On a conflict it falls back to `git rebase` for
//! that branch and records a journal so `--continue` / `--abort` work.
//! A change checked out in another worktree is rewritten there, so that
//! worktree ends up on the new tip.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::git::{Git, Replay};
use crate::model::{Base, State};
use crate::store::Repo;
use crate::worktree::{self, same_path};

#[derive(Debug, Serialize, Deserialize)]
pub struct Journal {
    /// Changes still to restack; the front is next.
    pub queue: Vec<String>,
    pub original_tips: BTreeMap<String, String>,
    pub original_forks: BTreeMap<String, Option<String>>,
    /// The change `git rebase` is working on, and the commit it goes onto.
    pub pending: Option<Pending>,
    /// The worktree the restack started in. Changes not checked out anywhere
    /// else are rebased here.
    pub home: PathBuf,
    /// Branch (or commit) to check out in `home` when done.
    pub return_to: String,
    /// Changes left alone because a worktree has them checked out dirty.
    #[serde(default)]
    pub blocked: Vec<Blocked>,
}

/// A change that couldn't be rewritten under its worktree, and the changes
/// stacked on it, which were left alone with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blocked {
    pub change: String,
    pub above: Vec<String>,
    pub why: String,
}

impl Blocked {
    pub fn describe(&self) -> String {
        if self.above.is_empty() {
            format!("skipped {}: {}", self.change, self.why)
        } else {
            format!(
                "skipped {} (and {} on top of it): {}",
                self.change,
                self.above.join(", "),
                self.why
            )
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Pending {
    pub change: String,
    pub onto: String,
    /// The worktree the rebase runs in.
    pub worktree: PathBuf,
}

#[derive(Debug, Default)]
pub struct Report {
    pub moved: Vec<String>,
    pub unchanged: usize,
    pub blocked: Vec<Blocked>,
}

pub enum Outcome {
    Done(Report),
    /// `git rebase` stopped on a conflict in this change. `worktree` is where,
    /// when that isn't the worktree we're running in.
    Conflict {
        change: String,
        worktree: Option<PathBuf>,
    },
}

impl Outcome {
    fn conflict(git: &Git, change: &str, dir: &std::path::Path) -> Outcome {
        let here = git.toplevel().is_ok_and(|t| same_path(&t, dir));
        Outcome::Conflict {
            change: change.to_string(),
            worktree: (!here).then(|| dir.to_path_buf()),
        }
    }
}

/// `"x"`, or `"x" (checked out in /path)` for a conflict in another worktree.
pub fn conflict_place(change: &str, worktree: &Option<PathBuf>) -> String {
    match worktree {
        Some(p) => format!("{change:?} (checked out in {})", p.display()),
        None => format!("{change:?}"),
    }
}

/// The commit trunk-based stacks sit on: the remote-tracking trunk when there
/// is one (so PR diffs match GitHub), else the local trunk.
pub fn trunk_tip(git: &Git, st: &State) -> Result<String> {
    git.resolve(&format!("refs/remotes/{}/{}", st.remote, st.trunk))
        .or_else(|| git.resolve(&format!("refs/heads/{}", st.trunk)))
        .with_context(|| format!("trunk branch {:?} doesn't exist", st.trunk))
}

/// The commit a change should sit on right now.
pub fn parent_tip(git: &Git, st: &State, name: &str) -> Result<String> {
    match st
        .parent(name)
        .with_context(|| format!("{name:?} isn't in any stack"))?
    {
        Base::Trunk => trunk_tip(git, st),
        Base::Change(p) => git.branch_tip(&p),
        Base::Sum(ms) => sum_commit(git, &ms),
    }
}

/// The synthetic merge of several changes that a parked stack sits on. It's
/// deterministic, so unchanged inputs give the same commit and nothing moves.
pub fn sum_commit(git: &Git, members: &[String]) -> Result<String> {
    let tips = members
        .iter()
        .map(|m| git.branch_tip(m))
        .collect::<Result<Vec<_>>>()?;
    let mut time = 0;
    for t in &tips {
        time = time.max(git.committer_time(t)?);
    }
    let msg = format!("gh-dags sum: {}", members.join(" ⊕ "));
    let mut acc = tips[0].clone();
    let mut tree = String::new();
    for (i, t) in tips.iter().enumerate().skip(1) {
        tree = git.merge_tree(&acc, t)?.with_context(|| {
            format!(
                "can't build the sum {}: {} conflicts with the rest, so they aren't independent; \
                 linearize instead (gh dags move)",
                members.join(" ⊕ "),
                members[i]
            )
        })?;
        acc = git.commit_tree(&tree, &[acc.clone(), t.clone()], &msg, time)?;
    }
    if tips.len() == 2 {
        return Ok(acc);
    }
    git.commit_tree(&tree, &tips, &msg, time)
}

/// Restack every change.
/// Which changes a restack touches.
#[derive(Debug, Default)]
pub struct Scope {
    /// Only these changes; all of them when `None`.
    pub only: Option<BTreeSet<String>>,
    /// Leave changes that sit on trunk where they are, so only changes move
    /// relative to each other.
    pub no_trunk: bool,
}

/// Restack the changes in `scope`, each onto its parent's current tip.
pub fn start(repo: &Repo, st: &mut State, scope: &Scope) -> Result<Outcome> {
    let git = &repo.git;
    if repo.has_journal() {
        bail!("a restack is already in progress; run `gh dags restack --continue` or `--abort`");
    }
    if git.rebase_in_progress() {
        bail!("a git rebase is in progress; finish or abort it first");
    }
    if !git.is_clean()? {
        bail!("you have uncommitted changes; commit or stash them first");
    }
    let queue: Vec<String> = st
        .topo_order()?
        .into_iter()
        .filter(|n| scope.only.as_ref().is_none_or(|only| only.contains(n)))
        .filter(|n| !(scope.no_trunk && st.parent(n) == Some(Base::Trunk)))
        .collect();
    trunk_tip(git, st)?;

    // A change whose parent is outside the scope goes onto that parent as it
    // is. If the parent (or something under it) hasn't been restacked since
    // the layout changed, it can still hold some of the change's own
    // commits, and replaying those onto it would drop them.
    for name in &queue {
        let bases = match st.parent(name) {
            Some(Base::Change(p)) => vec![p],
            Some(Base::Sum(ms)) => ms,
            _ => continue,
        };
        let outside: Vec<String> = bases.into_iter().filter(|b| !queue.contains(b)).collect();
        if outside.is_empty() {
            continue;
        }
        let Some(fork) = st.changes[name]
            .fork
            .clone()
            .filter(|f| git.resolve(f).is_some())
        else {
            continue;
        };
        let onto = parent_tip(git, st, name)?;
        let tip = git.branch_tip(name)?;
        if !in_place(git, &onto, &tip, Some(&fork))
            && let Some(mb) = git.merge_base(&tip, &onto)
            && mb != fork
            && git.is_ancestor(&fork, &mb)
        {
            bail!(
                "{name} can't go on {stale} yet: it still has {name}'s commits from before the \
                 layout changed; restack what's below {name} first \
                 (`gh dags restack {name} --downstack`)",
                stale = outside.join(", ")
            );
        }
    }

    let mut original_tips = BTreeMap::new();
    for name in &queue {
        original_tips.insert(name.clone(), git.branch_tip(name)?);
    }
    let journal = Journal {
        original_forks: queue
            .iter()
            .map(|n| (n.clone(), st.changes[n].fork.clone()))
            .collect(),
        queue,
        original_tips,
        pending: None,
        home: git.toplevel()?,
        return_to: git.head()?,
        blocked: Vec::new(),
    };
    if journal.queue.contains(&journal.return_to) {
        // Rewriting the checked-out branch under the worktree; step off it.
        git.run(&["checkout", "--quiet", "--detach"])?;
    }
    repo.save_journal(&journal)?;
    run_queue(repo, st, journal, Report::default())
}

/// `restack --continue`: finish the interrupted rebase, then the rest.
pub fn resume(repo: &Repo, st: &mut State) -> Result<Outcome> {
    let git = &repo.git;
    let mut journal: Journal = repo.load_journal()?.context("no restack is in progress")?;
    let mut report = Report::default();
    if let Some(p) = journal.pending.take() {
        let wt = Git::new(&p.worktree);
        if wt.rebase_in_progress() {
            let ok = wt.run_visible(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])?;
            if !ok {
                let outcome = Outcome::conflict(git, &p.change, &p.worktree);
                journal.pending = Some(p);
                repo.save_journal(&journal)?;
                return Ok(outcome);
            }
        }
        if !git.is_ancestor(&p.onto, &git.branch_tip(&p.change)?) {
            bail!(
                "the rebase of {:?} didn't finish; run `gh dags restack --abort` to undo the restack",
                p.change
            );
        }
        st.change_mut(&p.change)?.fork = Some(p.onto);
        journal.queue.retain(|n| *n != p.change);
        report.moved.push(p.change);
        repo.save(st)?;
        repo.save_journal(&journal)?;
    }
    run_queue(repo, st, journal, report)
}

/// `restack --abort`: put every branch and fork point back.
pub fn abort(repo: &Repo, st: &mut State) -> Result<()> {
    let git = &repo.git;
    let journal: Journal = repo.load_journal()?.context("no restack is in progress")?;
    let mut moved = Vec::new();
    for (name, orig) in &journal.original_tips {
        if let Ok(cur) = git.branch_tip(name)
            && cur != *orig
        {
            moved.push((name, orig, cur));
        }
    }
    // Check every worktree that has to follow its branch back before
    // touching any of them.
    for (name, _, _) in &moved {
        if let Some(path) = worktree::checkout_of(git, name)?
            && let Some(why) = worktree::blocker(git, name, &path)
        {
            bail!("can't abort yet: {why}");
        }
    }
    if let Some(p) = &journal.pending {
        let wt = Git::new(&p.worktree);
        if wt.rebase_in_progress() {
            wt.run(&["rebase", "--abort"])?;
        }
    }
    for (name, orig, cur) in moved {
        worktree::move_branch(git, name, orig, &cur)?;
    }
    for (name, fork) in journal.original_forks {
        if let Ok(c) = st.change_mut(&name) {
            c.fork = fork;
        }
    }
    repo.save(st)?;
    Git::new(&journal.home).run(&["checkout", "--quiet", &journal.return_to])?;
    repo.clear_journal()
}

/// Whether any change isn't sitting on its parent's current tip.
pub fn needs_restack(git: &Git, st: &State) -> Result<bool> {
    for name in st.topo_order()? {
        let onto = parent_tip(git, st, &name)?;
        let tip = git.branch_tip(&name)?;
        let fork = st
            .change(&name)?
            .fork
            .clone()
            .filter(|f| git.resolve(f).is_some());
        if !in_place(git, &onto, &tip, fork.as_deref()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the branch's own commits already sit directly on `onto`. Having
/// `onto` in its history isn't enough: a change moved down (off `a`, onto
/// trunk) still has trunk in its history, but also `a`'s commits, which it
/// must drop. Those show up as a fork point that is in the branch's history
/// but not below `onto`.
fn in_place(git: &Git, onto: &str, tip: &str, fork: Option<&str>) -> bool {
    if !git.is_ancestor(onto, tip) {
        return false;
    }
    match fork {
        None => true,
        // A fork no longer in the history means the branch was rebased by hand.
        Some(f) => !git.is_ancestor(f, tip) || git.is_ancestor(f, onto),
    }
}

fn run_queue(
    repo: &Repo,
    st: &mut State,
    mut journal: Journal,
    mut report: Report,
) -> Result<Outcome> {
    let git = &repo.git;
    let home = Git::new(&journal.home);
    while let Some(name) = journal.queue.first().cloned() {
        let onto = parent_tip(git, st, &name)?;
        let tip = git.branch_tip(&name)?;
        let fork = st
            .change(&name)?
            .fork
            .clone()
            .filter(|f| git.resolve(f).is_some());
        if in_place(git, &onto, &tip, fork.as_deref()) {
            report.unchanged += 1;
        } else {
            let holder = worktree::checkout_of(git, &name)?;
            if let Some(why) = holder
                .as_ref()
                .and_then(|p| worktree::blocker(git, &name, p))
            {
                // Leave it, and what's stacked on it, for a later restack.
                let held = st.subtree(&name);
                let above = journal.queue[1..]
                    .iter()
                    .filter(|n| held.contains(*n))
                    .cloned()
                    .collect();
                journal.queue.retain(|n| !held.contains(n));
                journal.blocked.push(Blocked {
                    change: name,
                    above,
                    why,
                });
                repo.save_journal(&journal)?;
                continue;
            }
            let fork = match fork {
                Some(f) => f,
                None => git.merge_base(&tip, &onto).with_context(|| {
                    format!("{name:?} has no history in common with its parent")
                })?,
            };
            // Gits older than the replay we test against take the `git
            // rebase` path, and GH_DAGS_NO_REPLAY forces it: for testing it
            // against replay, and for gits whose replay misbehaves.
            let replayed =
                if std::env::var_os("GH_DAGS_NO_REPLAY").is_some() || !crate::git::can_replay() {
                    Replay::Failed
                } else {
                    git.replay(&onto, &fork, &name)?
                };
            match replayed {
                Replay::Done(new) => worktree::move_branch(git, &name, &new, &tip)?,
                Replay::Empty => worktree::move_branch(git, &name, &onto, &tip)?,
                Replay::Failed => {
                    // Rebase where the change is checked out, so that
                    // worktree ends up on the result; anything else, at home.
                    let dir = holder.unwrap_or_else(|| journal.home.clone());
                    journal.pending = Some(Pending {
                        change: name.clone(),
                        onto: onto.clone(),
                        worktree: dir.clone(),
                    });
                    repo.save(st)?;
                    repo.save_journal(&journal)?;
                    let wt = Git::new(&dir);
                    if !wt.run_visible(&["rebase", "--onto", &onto, &fork, &name], &[])? {
                        return Ok(Outcome::conflict(git, &name, &dir));
                    }
                    journal.pending = None;
                }
            }
            report.moved.push(name.clone());
        }
        st.change_mut(&name)?.fork = Some(onto);
        journal.queue.remove(0);
        repo.save(st)?;
        repo.save_journal(&journal)?;
    }
    home.run(&["checkout", "--quiet", &journal.return_to])?;
    repo.clear_journal()?;
    report.blocked = journal.blocked;
    Ok(Outcome::Done(report))
}
