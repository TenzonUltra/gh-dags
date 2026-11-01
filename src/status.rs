//! Where each change stands right now, for `view`: its branch tip, whether a
//! restack would move it, and the worktree it's checked out in. It runs on
//! every `view`, so it reads every tip in one call and asks git about
//! ancestry only where a fork point disagrees with its parent's tip.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::git::Git;
use crate::model::{Base, State};
use crate::restack;
use crate::store::Repo;
use crate::worktree::same_path;

pub struct Status {
    pub head: String,
    /// Its parent (a change, or the changes it's parked on) moved since it
    /// was last restacked.
    pub parent_moved: bool,
    /// It sits on trunk, and trunk has moved on.
    pub behind_trunk: bool,
    /// A plain restack would move it: it, or something below it, is out of
    /// place.
    pub needs_restack: bool,
    /// Checked out in another worktree.
    pub worktree: Option<PathBuf>,
    /// That worktree has uncommitted changes.
    pub dirty: bool,
    /// That worktree's directory is gone.
    pub missing: bool,
}

#[derive(Default)]
pub struct Statuses {
    /// Where `view` runs, for showing other worktrees' paths relative to it.
    pub here: Option<PathBuf>,
    pub of: BTreeMap<String, Status>,
}

pub fn statuses(repo: &Repo, st: &State) -> Result<Statuses> {
    let git = &repo.git;
    // Every tip, and the trunk the way restacks pick it, in one call.
    let remote_trunk = format!("refs/remotes/{}/{}", st.remote, st.trunk);
    let refs = git.refs(&["refs/heads", &remote_trunk])?;
    let trunk = refs
        .get(&remote_trunk)
        .or_else(|| refs.get(&format!("refs/heads/{}", st.trunk)))
        .cloned();
    let tips: BTreeMap<String, String> = refs
        .into_iter()
        .filter_map(|(r, sha)| Some((r.strip_prefix("refs/heads/")?.to_string(), sha)))
        .collect();
    // Other worktrees, when there are any.
    let mut here = None;
    let mut holders = BTreeMap::new();
    if repo.has_linked_worktrees() {
        let trees = git.worktrees()?;
        // This one is the deepest that contains the current directory.
        let cwd = std::env::current_dir()?.canonicalize()?;
        let this = trees
            .iter()
            .filter_map(|w| w.path.canonicalize().ok())
            .filter(|p| cwd.starts_with(p))
            .max_by_key(|p| p.components().count());
        for w in trees {
            if let Some(b) = w.branch
                && st.has(&b)
                && !this.as_ref().is_some_and(|h| same_path(h, &w.path))
            {
                holders.insert(b, w.path);
            }
        }
        here = Some(cwd);
    }
    // `git status` in each worktree is the slow part, so ask them all at once.
    let dirty: BTreeMap<String, bool> = std::thread::scope(|scope| {
        let asks: Vec<_> = holders
            .iter()
            .filter(|(_, p)| p.is_dir())
            .map(|(b, p)| {
                (
                    b,
                    scope.spawn(move || !Git::new(p).is_clean().unwrap_or(true)),
                )
            })
            .collect();
        asks.into_iter()
            .map(|(b, ask)| (b.clone(), ask.join().unwrap_or(false)))
            .collect()
    });
    let mut of: BTreeMap<String, Status> = BTreeMap::new();
    for name in st.topo_order()? {
        let Some(head) = tips.get(&name).cloned() else {
            continue;
        };
        let fork = st.changes[&name].fork.as_deref();
        let parent = st.parent(&name);
        let (parent_moved, behind_trunk) = match &parent {
            Some(Base::Trunk) => (
                false,
                trunk
                    .as_ref()
                    .is_some_and(|t| !sits_on(git, t, &head, fork)),
            ),
            Some(Base::Change(p)) => (
                tips.get(p).is_some_and(|t| !sits_on(git, t, &head, fork)),
                false,
            ),
            Some(Base::Sum(ms)) => (!sum_current(git, &tips, ms, fork), false),
            None => (false, false),
        };
        let below = match &parent {
            Some(Base::Change(p)) => of.get(p).is_some_and(|s| s.needs_restack),
            Some(Base::Sum(ms)) => ms
                .iter()
                .any(|m| of.get(m).is_some_and(|s| s.needs_restack)),
            _ => false,
        };
        let worktree = holders.get(&name).cloned();
        let missing = worktree.as_ref().is_some_and(|p| !p.is_dir());
        let dirty = dirty.get(&name).copied().unwrap_or(false);
        of.insert(
            name,
            Status {
                head,
                parent_moved,
                behind_trunk,
                needs_restack: parent_moved || behind_trunk || below,
                worktree,
                dirty,
                missing,
            },
        );
    }
    Ok(Statuses { here, of })
}

/// Whether a change at `head`, with fork point `fork`, sits on `onto` the way
/// a restack would leave it. A restack sets the fork point to the parent's
/// tip, which settles the common case without asking git.
fn sits_on(git: &Git, onto: &str, head: &str, fork: Option<&str>) -> bool {
    fork == Some(onto) || restack::in_place(git, onto, head, fork)
}

/// Whether the sum a parked change sits on is still the merge of its
/// members' current tips: a sum commit's parents are those tips.
fn sum_current(
    git: &Git,
    tips: &BTreeMap<String, String>,
    ms: &[String],
    fork: Option<&str>,
) -> bool {
    let Some(fork) = fork else {
        return false;
    };
    let parents = git
        .run(&["rev-list", "--parents", "-n1", fork])
        .unwrap_or_default();
    let parents: Vec<&str> = parents.split_whitespace().skip(1).collect();
    parents.len() == ms.len()
        && ms
            .iter()
            .zip(&parents)
            .all(|(m, p)| tips.get(m).is_some_and(|t| t == p))
}

/// `to` relative to `from` when that's shorter, else as it is.
pub fn shorter(from: &Path, to: &Path) -> String {
    let full = to.display().to_string();
    let (Ok(a), Ok(b)) = (from.canonicalize(), to.canonicalize()) else {
        return full;
    };
    let a: Vec<Component> = a.components().collect();
    let b: Vec<Component> = b.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if common == 0 {
        return full;
    }
    let mut rel = PathBuf::new();
    for _ in common..a.len() {
        rel.push("..");
    }
    for c in &b[common..] {
        rel.push(c);
    }
    let rel = rel.display().to_string();
    if rel.len() < full.len() { rel } else { full }
}
