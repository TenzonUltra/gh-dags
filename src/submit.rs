//! The GitHub projection (DESIGN.md §6–7): `submit` pushes branches and
//! makes PRs and stacks match the layout, `sync` absorbs what happened on
//! GitHub, and `land` hoists a change and merges it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::json;

use crate::github::{Gh, MergeStatus, PrInfo, RemoteStack, status_of};
use crate::layout;
use crate::model::{Base, State};
use crate::render::Style;
use crate::restack::{self, Outcome};
use crate::store::Repo;

const START: &str = "<!-- gh-dags:start -->";
const END: &str = "<!-- gh-dags:end -->";

pub struct Opts {
    /// Open new PRs ready for review instead of as drafts.
    pub ready: bool,
    /// Create PRs for changes that don't have one (sync only updates).
    pub create: bool,
}

/// What one layout stack should look like on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    /// Index into `State::stacks`.
    pub stack: usize,
    pub base: String,
    pub prs: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Keep(u64),
    Append(u64, Vec<u64>),
    Create(Vec<u64>),
    /// Fewer than two PRs: GitHub stacks need at least two.
    Plain,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Remote stacks to dissolve first (their bases can't change while stacked).
    pub unstack: BTreeSet<u64>,
    pub actions: Vec<(usize, Action)>,
}

/// Decide, per layout stack, whether its GitHub stack can stay, only needs
/// appending (cheap), or has to be rebuilt (unstack, retarget, create).
pub fn plan_stacks(desired: &[Desired], remote: &[RemoteStack]) -> Plan {
    let mut owner: BTreeMap<u64, &RemoteStack> = BTreeMap::new();
    for r in remote {
        for (n, open) in &r.prs {
            if *open {
                owner.insert(*n, r);
            }
        }
    }
    let mut plan = Plan::default();
    for d in desired {
        let touching: BTreeMap<u64, &RemoteStack> = d
            .prs
            .iter()
            .filter_map(|n| owner.get(n).map(|r| (r.number, *r)))
            .collect();
        if touching.len() == 1 {
            let r = touching.values().next().unwrap();
            let open: Vec<u64> = r.prs.iter().filter(|(_, o)| *o).map(|(n, _)| *n).collect();
            if r.base == d.base && d.prs.starts_with(&open) {
                let action = if open.len() == d.prs.len() {
                    Action::Keep(r.number)
                } else {
                    Action::Append(r.number, d.prs[open.len()..].to_vec())
                };
                plan.actions.push((d.stack, action));
                continue;
            }
        }
        plan.unstack.extend(touching.keys());
        let action = if d.prs.len() >= 2 {
            Action::Create(d.prs.clone())
        } else {
            Action::Plain
        };
        plan.actions.push((d.stack, action));
    }
    plan
}

/// The block gh-dags owns in each PR body: the real dependencies, which the
/// stack map alone can't show, plus machine-readable metadata.
pub fn annotation(st: &State, name: &str) -> String {
    let refer = |c: &str| match st.changes.get(c).and_then(|c| c.pr) {
        Some(n) => format!("#{n} (`{c}`)"),
        None => format!("`{c}`"),
    };
    let change = &st.changes[name];
    let mut text = if change.deps.is_empty() {
        format!("Depends only on `{}`.", st.trunk)
    } else {
        let deps: Vec<String> = change.deps.iter().map(|d| refer(d)).collect();
        format!("Depends on {}.", deps.join(", "))
    };
    if let Some(p) = st.ordering_only_parent(name) {
        text.push_str(&format!(
            " Stacked on {} for ordering only: it doesn't need that change.",
            refer(&p)
        ));
    }
    let meta = json!({"v": 1, "change": name, "deps": change.deps});
    format!("{START}\n---\n**gh-dags** · {text}\n<!-- gh-dags:meta {meta} -->\n{END}")
}

/// The machine-readable line in gh-dags' block: which change a PR is, and
/// what it depends on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Meta {
    pub v: u32,
    pub change: String,
    #[serde(default)]
    pub deps: BTreeSet<String>,
}

/// Read the metadata back out of a PR body. `None` if the PR wasn't made by
/// gh-dags, or by a version whose format this one doesn't know.
pub fn parse_meta(body: &str) -> Option<Meta> {
    let s = body.find(START)?;
    let e = s + body[s..].find(END)?;
    let line = body[s..e].lines().find_map(|l| {
        l.trim()
            .strip_prefix("<!-- gh-dags:meta ")?
            .strip_suffix(" -->")
    })?;
    let meta: Meta = serde_json::from_str(line).ok()?;
    (meta.v == 1).then_some(meta)
}

/// Replace gh-dags' block in a PR body, or append it. The rest of the body
/// is left alone.
pub fn upsert_block(body: &str, block: &str) -> String {
    if let (Some(s), Some(e)) = (body.find(START), body.find(END))
        && s < e
    {
        return format!("{}{block}{}", &body[..s], &body[e + END.len()..]);
    }
    let body = body.trim_end();
    if body.is_empty() {
        block.to_string()
    } else {
        format!("{body}\n\n{block}")
    }
}

fn base_branch(st: &State, name: &str) -> String {
    match st.parent(name) {
        Some(Base::Change(p)) => p,
        _ => st.trunk.clone(),
    }
}

/// Each submittable layout stack, as the run of its changes that have PRs.
fn desired_stacks(st: &State) -> Vec<Desired> {
    let mut out = Vec::new();
    for (si, s) in st.stacks.iter().enumerate() {
        if st.is_parked(&s.changes[0]) {
            continue;
        }
        let prs: Vec<u64> = s.changes.iter().map_while(|c| st.changes[c].pr).collect();
        if !prs.is_empty() {
            out.push(Desired {
                stack: si,
                base: base_branch(st, &s.changes[0]),
                prs,
            });
        }
    }
    out
}

fn ensure_restacked(repo: &Repo, st: &mut State) -> Result<()> {
    if restack::needs_restack(&repo.git, st)? {
        match restack::start(repo, st, &restack::Scope::default())? {
            Outcome::Done(r) => println!("restacked {}", r.moved.join(", ")),
            Outcome::Conflict(name) => bail!(
                "restack stopped on a conflict in {name:?}; resolve it and run \
                 `gh dags restack --continue`, then submit again"
            ),
        }
    }
    Ok(())
}

pub fn submit(repo: &Repo, st: &mut State, gh: &Gh, opts: Opts, style: &Style) -> Result<()> {
    let git = &repo.git;
    ensure_restacked(repo, st)?;
    let order: Vec<String> = st
        .topo_order()?
        .into_iter()
        .filter(|c| !st.is_parked(c))
        .collect();
    if order.is_empty() {
        println!("nothing to submit");
        return Ok(());
    }

    // Order matters here. GitHub marks a PR as merged the moment its base
    // branch contains its head, so pushing a reordered stack while PRs still
    // have their old bases "merges" them into each other. Trunk never
    // contains unmerged work, so every PR whose base changes goes to trunk
    // first, and only gets its real base after the push.

    // 1. What needs pushing, leased on what we last pushed.
    let remote_tips = git.remote_tips(&st.remote, &order)?;
    let mut leases = Vec::new();
    let mut stale = Vec::new();
    for name in &order {
        let local = git.branch_tip(name)?;
        let remote = remote_tips.get(name);
        if remote == Some(&local) {
            st.change_mut(name)?.pushed = Some(local);
            continue;
        }
        let pushed = st.changes[name].pushed.as_ref();
        match (remote, pushed) {
            (Some(r), Some(p)) if r != p => stale.push(name.clone()),
            (Some(r), None) if !git.is_ancestor(r, &local) => stale.push(name.clone()),
            _ => leases.push((name.clone(), remote.cloned().unwrap_or_default())),
        }
    }
    if !stale.is_empty() {
        bail!(
            "{} changed on the remote since gh-dags last pushed (a GitHub rebase or someone \
             else's push); run `gh dags sync` to take those changes first",
            stale.join(", ")
        );
    }

    // 2. The PRs that exist: recorded ones still open, or one already open
    //    for the branch.
    let known: Vec<u64> = order.iter().filter_map(|n| st.changes[n].pr).collect();
    let mut info: BTreeMap<u64, PrInfo> = gh.prs(&known)?;
    for name in &order {
        if let Some(n) = st.changes[name].pr {
            match info.get(&n).map(|p| p.state.as_str()) {
                Some("OPEN") => continue,
                Some("MERGED") => bail!("#{n} ({name}) has merged; run `gh dags sync` first"),
                _ => st.change_mut(name)?.pr = None,
            }
        }
        if let Some(n) = gh.open_pr_for(name)? {
            st.change_mut(name)?.pr = Some(n);
            info.extend(gh.prs(&[n])?);
        }
    }

    // 3. Free every PR whose base changes: out of its stack, onto trunk.
    //    A stacked PR's base can't change, so any stack holding one is
    //    dissolved, whatever the plan says (the plan only sees each layout
    //    stack up to its first change without a PR).
    let mut moving = Vec::new();
    for name in &order {
        let Some(pr) = st.changes[name].pr.and_then(|n| info.get(&n)) else {
            continue;
        };
        let base = base_branch(st, name);
        if pr.base != base {
            moving.push((name.clone(), pr.number, pr.base.clone(), base));
        }
    }
    let remote = gh.stacks()?;
    let mut unstack = plan_stacks(&desired_stacks(st), &remote).unstack;
    for (_, n, _, _) in &moving {
        if let Some(r) = remote.iter().find(|r| r.prs.contains(&(*n, true))) {
            unstack.insert(r.number);
        }
    }
    for number in &unstack {
        gh.unstack(*number)?;
        println!(
            "dissolved GitHub stack {} to rebuild it",
            style.cyan(&format!("#{number}"))
        );
    }
    for (_, n, old, _) in &moving {
        if *old != st.trunk {
            gh.update_pr(*n, json!({"base": st.trunk}))?;
        }
    }

    // 4. Push.
    if !leases.is_empty() {
        git.push_leased(&st.remote, &leases)?;
        for (name, _) in &leases {
            st.change_mut(name)?.pushed = Some(git.branch_tip(name)?);
        }
        repo.save(st)?;
        let names: Vec<&str> = leases.iter().map(|(n, _)| n.as_str()).collect();
        println!("pushed {}", names.join(", "));
    }

    // 5. Open PRs for changes without one, then give moved PRs their bases.
    for name in &order {
        if st.changes[name].pr.is_some() || !opts.create {
            continue;
        }
        let base = base_branch(st, name);
        let fork = st.changes[name].fork.clone().unwrap_or(base.clone());
        let Some(title) = git
            .subjects(&fork, &format!("refs/heads/{name}"))?
            .into_iter()
            .next()
        else {
            println!(
                "{} {name} has no commits yet, so it has no PR",
                style.dim("skipped:")
            );
            continue;
        };
        let body = annotation(st, name);
        let (n, url) = gh.create_pr(name, &base, &title, &body, !opts.ready)?;
        println!(
            "opened {} {name} → {base}  {}",
            style.cyan(&format!("#{n}")),
            style.dim(&url)
        );
        st.change_mut(name)?.pr = Some(n);
        repo.save(st)?;
        info.extend(gh.prs(&[n])?);
    }
    for (name, n, _, base) in &moving {
        if *base != st.trunk {
            gh.update_pr(*n, json!({"base": base}))?;
        }
        println!("retargeted #{n} {name} → {base}");
    }

    // 6. Annotations, now that every PR number is known.
    for name in &order {
        let Some(n) = st.changes[name].pr else {
            continue;
        };
        let Some(pr) = info.get(&n) else { continue };
        let body = upsert_block(&pr.body, &annotation(st, name));
        if body != pr.body {
            gh.update_pr(n, json!({"body": body}))?;
        }
    }

    // 7. Stacks: create or append. Anything that had to be dissolved already was.
    let plan = plan_stacks(&desired_stacks(st), &gh.stacks()?);
    for number in &plan.unstack {
        gh.unstack(*number)?;
    }
    for (si, action) in plan.actions {
        let label = |prs: &[u64]| {
            prs.iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(" ← ")
        };
        match action {
            Action::Keep(number) => st.stacks[si].github = Some(number),
            Action::Append(number, prs) => {
                gh.add_to_stack(number, &prs)?;
                st.stacks[si].github = Some(number);
                println!(
                    "stack {}: added {}",
                    style.cyan(&format!("#{number}")),
                    label(&prs)
                );
            }
            Action::Create(prs) => match gh.create_stack(&prs) {
                Ok(number) => {
                    st.stacks[si].github = Some(number);
                    println!(
                        "stack {}: {}",
                        style.cyan(&format!("#{number}")),
                        label(&prs)
                    );
                }
                Err(e) if status_of(&e) == Some(422) => {
                    st.stacks[si].github = None;
                    println!(
                        "{} GitHub didn't accept a stack for {}, so they stay plain PRs: {e}",
                        style.yellow("note:"),
                        label(&prs)
                    );
                }
                Err(e) => return Err(e),
            },
            Action::Plain => st.stacks[si].github = None,
        }
    }
    repo.save(st)?;
    Ok(())
}

pub fn sync(repo: &Repo, st: &mut State, gh: &Gh, style: &Style) -> Result<()> {
    let git = &repo.git;
    git.run(&["fetch", "--quiet", "--prune", &st.remote])?;

    // Merged changes leave the DAG, in physical order (GitHub merges prefixes).
    let numbers: Vec<u64> = st.changes.values().filter_map(|c| c.pr).collect();
    let info = gh.prs(&numbers)?;
    let trunk = git.resolve(&format!("refs/remotes/{}/{}", st.remote, st.trunk));
    let parked_before: BTreeSet<String> = st
        .changes
        .keys()
        .filter(|c| st.is_parked(c))
        .cloned()
        .collect();
    let mut merged = Vec::new();
    for name in st.topo_order()? {
        let Some(n) = st.changes[&name].pr else {
            continue;
        };
        let Some(pr) = info.get(&n) else { continue };
        // Landed means the merge commit is in trunk. A stack merge leaves the
        // upper PRs based on the branch below them, so the base can't tell.
        let landed = match &pr.merge_commit {
            Some(c) => trunk.as_ref().is_some_and(|t| git.is_ancestor(c, t)),
            None => pr.base == st.trunk,
        };
        match pr.state.as_str() {
            "MERGED" if landed => match layout::absorb_merged(st, &name) {
                Ok(()) => merged.push(name),
                Err(e) => println!(
                    "{} #{n} ({name}) merged, but {e:#}",
                    style.yellow("warning:")
                ),
            },
            // Merged somewhere other than trunk, usually GitHub's implicit
            // merge (the base came to contain the head). The change itself
            // hasn't landed, so it gets a new PR.
            "MERGED" => {
                println!(
                    "{} #{n} ({name}) was marked merged into {}, but not into {}; it'll get a new PR on the next submit",
                    style.yellow("warning:"),
                    pr.base,
                    st.trunk
                );
                st.change_mut(&name)?.pr = None;
            }
            "CLOSED" => println!(
                "{} #{n} ({name}) was closed without merging",
                style.yellow("warning:")
            ),
            _ => {}
        }
    }
    for name in &merged {
        println!("merged: {name} (delete the branch with `git branch -D {name}`)");
    }
    // sync only updates PRs; opening new ones is submit's job.
    for name in parked_before
        .iter()
        .filter(|c| st.has(c) && !st.is_parked(c))
    {
        println!("{name} isn't parked any more; `gh dags submit` opens its PR");
    }

    // Take remote rewrites (e.g. GitHub's rebase after a merge) when we
    // haven't changed the branch since our last push.
    let mut diverged = Vec::new();
    for name in st.topo_order()? {
        let Some(pushed) = st.changes[&name].pushed.clone() else {
            continue;
        };
        let Some(remote) = git.resolve(&format!("refs/remotes/{}/{name}", st.remote)) else {
            continue;
        };
        let local = git.branch_tip(&name)?;
        if remote == pushed || remote == local {
            continue;
        }
        if local != pushed {
            diverged.push(name);
            continue;
        }
        restack::move_branch(git, &name, &remote, &local)?;
        let onto = restack::parent_tip(git, st, &name)?;
        let fork = if git.is_ancestor(&onto, &remote) {
            Some(onto)
        } else {
            git.merge_base(&remote, &onto)
        };
        let c = st.change_mut(&name)?;
        c.fork = fork;
        c.pushed = Some(remote);
        println!("took {name} from {} (it changed on GitHub)", st.remote);
    }
    fast_forward_trunk(repo, st, &merged)?;
    repo.save(st)?;
    if !diverged.is_empty() {
        bail!(
            "{} changed both here and on GitHub since the last push; reconcile by hand \
             (e.g. rebase onto {}/<branch>), then run `gh dags submit`",
            diverged.join(", "),
            st.remote
        );
    }

    if restack::needs_restack(git, st)? {
        match restack::start(repo, st, &restack::Scope::default())? {
            Outcome::Done(r) => println!("restacked {}", r.moved.join(", ")),
            Outcome::Conflict(name) => bail!(
                "restack stopped on a conflict in {name:?}; resolve it and run `gh dags restack --continue`"
            ),
        }
    }
    if st.changes.values().any(|c| c.pr.is_some()) {
        submit(
            repo,
            st,
            gh,
            Opts {
                ready: false,
                create: false,
            },
            style,
        )?;
    }
    println!("in sync with {}", st.remote);
    Ok(())
}

/// Bring local trunk up to the remote's, and step off a branch that merged.
fn fast_forward_trunk(repo: &Repo, st: &State, merged: &[String]) -> Result<()> {
    let git = &repo.git;
    let current = git.current_branch();
    let (Some(remote), Some(local)) = (
        git.resolve(&format!("refs/remotes/{}/{}", st.remote, st.trunk)),
        git.resolve(&format!("refs/heads/{}", st.trunk)),
    ) else {
        return Ok(());
    };
    let clean = git.is_clean()?;
    if local != remote && git.is_ancestor(&local, &remote) {
        if current.as_deref() == Some(st.trunk.as_str()) {
            if clean {
                git.run(&["merge", "--ff-only", "--quiet", &remote])?;
            }
        } else {
            git.update_branch(&st.trunk, &remote, &local)?;
        }
    }
    if let Some(c) = current
        && merged.contains(&c)
        && clean
    {
        git.run(&["checkout", "--quiet", &st.trunk])?;
        println!("{c} merged, so you're now on {}", st.trunk);
    }
    Ok(())
}

pub fn land(
    repo: &Repo,
    st: &mut State,
    gh: &Gh,
    change: &str,
    method: Option<&str>,
    style: &Style,
) -> Result<()> {
    let before = st.clone();
    if layout::hoist(st, change)? {
        for m in layout::describe_moves(&before, st) {
            println!("{} {m}", style.dim("layout:"));
        }
        repo.save(st)?;
        submit(
            repo,
            st,
            gh,
            Opts {
                ready: false,
                create: true,
            },
            style,
        )?;
        println!(
            "{change} and what it needs are now at the bottom of its stack. CI has to run on the \
             new heads; run `gh dags land {change}` again once it's green"
        );
        return Ok(());
    }

    // Merge only what GitHub has seen: everything up to `change`, pushed as-is.
    let (si, pos) = st.locate(change).unwrap();
    let prefix = st.stacks[si].changes[..=pos].to_vec();
    let mut current = !restack::needs_restack(&repo.git, st)?;
    for c in &prefix {
        let tip = repo.git.branch_tip(c)?;
        let ch = &st.changes[c];
        current &= ch.pr.is_some() && ch.pushed.as_deref() == Some(tip.as_str());
    }
    if !current {
        submit(
            repo,
            st,
            gh,
            Opts {
                ready: false,
                create: true,
            },
            style,
        )?;
        println!("pushed; run `gh dags land {change}` again once CI is green");
        return Ok(());
    }

    // GitHub won't merge drafts, and asking to land is asking to merge.
    let numbers: Vec<u64> = prefix.iter().filter_map(|c| st.changes[c].pr).collect();
    for pr in gh.prs(&numbers)?.values().filter(|p| p.draft) {
        gh.mark_ready(&pr.id)?;
        println!("marked #{} ready for review", pr.number);
    }
    let n = st.changes[change].pr.unwrap();
    println!("merging #{n} ({change}) and everything below it");
    let mut status = gh.merge(n, method)?;
    for _ in 0..100 {
        let MergeStatus::Pending(uuid) = &status else {
            break;
        };
        std::thread::sleep(Duration::from_secs(3));
        status = gh.merge_progress(n, uuid)?;
    }
    match status {
        MergeStatus::Merged => {
            println!("merged #{n}");
            sync(repo, st, gh, style)
        }
        MergeStatus::Enqueued => {
            println!("#{n} is in the merge queue; run `gh dags sync` once it lands");
            Ok(())
        }
        MergeStatus::Pending(_) => {
            println!("GitHub is still merging #{n}; run `gh dags sync` later");
            Ok(())
        }
        MergeStatus::Failed(msg) => bail!("GitHub didn't merge #{n}: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{FanIn, add_change};

    fn remote(number: u64, base: &str, prs: &[(u64, bool)]) -> RemoteStack {
        RemoteStack {
            number,
            base: base.into(),
            prs: prs.to_vec(),
        }
    }

    fn want(stack: usize, base: &str, prs: &[u64]) -> Desired {
        Desired {
            stack,
            base: base.into(),
            prs: prs.to_vec(),
        }
    }

    #[test]
    fn unchanged_stacks_are_kept_and_growth_is_appended() {
        let r = [remote(7, "main", &[(1, false), (2, true), (3, true)])];
        let plan = plan_stacks(&[want(0, "main", &[2, 3])], &r);
        assert_eq!(plan.actions, [(0, Action::Keep(7))]);
        assert!(plan.unstack.is_empty());

        let plan = plan_stacks(&[want(0, "main", &[2, 3, 4, 5])], &r);
        assert_eq!(plan.actions, [(0, Action::Append(7, vec![4, 5]))]);
        assert!(plan.unstack.is_empty());
    }

    #[test]
    fn reorders_and_rebases_rebuild() {
        let r = [remote(7, "main", &[(1, true), (2, true), (3, true)])];
        let plan = plan_stacks(&[want(0, "main", &[2, 1, 3])], &r);
        assert_eq!(plan.unstack, BTreeSet::from([7]));
        assert_eq!(plan.actions, [(0, Action::Create(vec![2, 1, 3]))]);

        // The stack's bottom moved onto another change (now a spur).
        let plan = plan_stacks(&[want(0, "auth", &[1, 2, 3])], &r);
        assert_eq!(plan.unstack, BTreeSet::from([7]));
    }

    #[test]
    fn splitting_a_stack_rebuilds_each_part() {
        let r = [remote(7, "main", &[(1, true), (2, true), (3, true)])];
        let plan = plan_stacks(&[want(0, "main", &[1, 2]), want(1, "1-branch", &[3])], &r);
        assert_eq!(plan.unstack, BTreeSet::from([7]));
        assert_eq!(
            plan.actions,
            [(0, Action::Create(vec![1, 2])), (1, Action::Plain)]
        );
    }

    #[test]
    fn merging_two_stacks_rebuilds_both() {
        let r = [
            remote(7, "main", &[(1, true), (2, true)]),
            remote(8, "main", &[(3, true), (4, true)]),
        ];
        let plan = plan_stacks(&[want(0, "main", &[1, 2, 3, 4])], &r);
        assert_eq!(plan.unstack, BTreeSet::from([7, 8]));
        assert_eq!(plan.actions, [(0, Action::Create(vec![1, 2, 3, 4]))]);
    }

    #[test]
    fn single_prs_stay_plain() {
        let plan = plan_stacks(&[want(0, "main", &[1])], &[]);
        assert_eq!(plan.actions, [(0, Action::Plain)]);
    }

    #[test]
    fn annotations_show_real_deps_and_ordering_only_edges() {
        let mut st = State::new("main", "origin");
        for (n, d) in [
            ("auth", vec![]),
            ("theme", vec![]),
            ("login", vec!["auth", "theme"]),
        ] {
            let d: Vec<String> = d.into_iter().map(String::from).collect();
            add_change(&mut st, n, &d, FanIn::Linearize).unwrap();
        }
        st.changes.get_mut("auth").unwrap().pr = Some(12);
        st.changes.get_mut("theme").unwrap().pr = Some(13);
        let theme = annotation(&st, "theme");
        assert!(
            theme.contains("Depends only on `main`. Stacked on #12 (`auth`) for ordering only"),
            "{theme}"
        );
        let login = annotation(&st, "login");
        assert!(
            login.contains("Depends on #12 (`auth`), #13 (`theme`)."),
            "{login}"
        );
        assert!(
            login.contains(
                r#"<!-- gh-dags:meta {"change":"login","deps":["auth","theme"],"v":1} -->"#
            ),
            "{login}"
        );
    }

    #[test]
    fn meta_reads_back_what_annotation_writes() {
        let mut st = State::new("main", "origin");
        for (n, d) in [
            ("auth", vec![]),
            ("theme", vec![]),
            ("login", vec!["auth", "theme"]),
        ] {
            let d: Vec<String> = d.into_iter().map(String::from).collect();
            add_change(&mut st, n, &d, FanIn::Linearize).unwrap();
        }
        let body = upsert_block("Some words.\n", &annotation(&st, "login"));
        let meta = parse_meta(&body).unwrap();
        assert_eq!(meta.change, "login");
        assert_eq!(meta.deps, st.changes["login"].deps);
        assert!(
            parse_meta(&annotation(&st, "auth"))
                .unwrap()
                .deps
                .is_empty()
        );

        assert_eq!(parse_meta("A PR made by hand."), None);
        let future = format!("{START}\n<!-- gh-dags:meta {{\"v\":2,\"change\":\"x\"}} -->\n{END}");
        assert_eq!(parse_meta(&future), None, "unknown versions are left alone");
        let outside = "<!-- gh-dags:meta {\"v\":1,\"change\":\"x\"} -->";
        assert_eq!(parse_meta(outside), None, "only gh-dags' own block counts");
    }

    #[test]
    fn upsert_replaces_only_our_block() {
        let block = format!("{START}\nnew\n{END}");
        assert_eq!(upsert_block("", &block), block);
        assert_eq!(
            upsert_block("Intro.\n", &block),
            format!("Intro.\n\n{block}")
        );
        let old = format!("Intro.\n\n{START}\nold\n{END}\n\nOutro.");
        assert_eq!(
            upsert_block(&old, &block),
            format!("Intro.\n\n{block}\n\nOutro.")
        );
    }
}
