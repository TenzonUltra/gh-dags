//! Branches checked out in worktrees. Moving a branch's ref under a worktree
//! that has it checked out would leave that worktree's index and files behind,
//! showing the rewrite as staged changes undoing it. So the worktree steps off
//! the branch, the ref moves, and it steps back on, which takes a clean tree.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git::Git;

pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// The worktree that has `branch` checked out, if any.
pub fn checkout_of(git: &Git, branch: &str) -> Result<Option<PathBuf>> {
    let mut holders = git
        .worktrees()?
        .into_iter()
        .filter(|w| w.branch.as_deref() == Some(branch))
        .map(|w| w.path);
    let first = holders.next();
    if let Some(second) = holders.next() {
        bail!(
            "{branch} is checked out in both {} and {}; switch one of them off it first",
            first.unwrap().display(),
            second.display()
        );
    }
    Ok(first)
}

/// Why `branch` can't be moved under the worktree at `path`, if it can't.
pub fn blocker(git: &Git, branch: &str, path: &Path) -> Option<String> {
    let at = match git.toplevel() {
        Ok(here) if same_path(&here, path) => "here".to_string(),
        _ => format!("in {}", path.display()),
    };
    if !path.is_dir() {
        return Some(format!(
            "{branch} is checked out {at}, which is gone; `git worktree prune` if you deleted it"
        ));
    }
    let wt = Git::new(path);
    if let Some(op) = wt.operation() {
        return Some(format!(
            "{branch} is checked out {at}, where a {op} is in progress; finish or abort it first"
        ));
    }
    if !wt.is_clean().unwrap_or(false) {
        return Some(format!(
            "{branch} is checked out {at} with uncommitted changes; commit or stash them first"
        ));
    }
    None
}

/// Point a branch at `new`, failing if it isn't at `old` any more. Wherever
/// it's checked out (here or in another worktree), that worktree follows it.
pub fn move_branch(git: &Git, branch: &str, new: &str, old: &str) -> Result<()> {
    let Some(path) = checkout_of(git, branch)? else {
        return git.update_branch(branch, new, old);
    };
    if let Some(why) = blocker(git, branch, &path) {
        bail!("{why}");
    }
    let wt = Git::new(&path);
    wt.run(&["checkout", "--quiet", "--detach"])?;
    let moved = wt.update_branch(branch, new, old);
    let back = wt.run(&["checkout", "--quiet", branch]);
    moved?;
    back.with_context(|| {
        format!(
            "moved {branch}, but couldn't check it out again in {}; run `git checkout {branch}` there",
            path.display()
        )
    })?;
    Ok(())
}
