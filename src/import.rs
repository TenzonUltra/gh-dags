//! Rebuilding gh-dags' state from what GitHub shows, so a DAG made on one
//! machine can be picked up on another. Each PR's annotation gives its
//! change's deps, its base gives the change's physical parent, and GitHub's
//! stacks say which child continues a stack and which starts a spur.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use crate::github::{Gh, RemoteStack};
use crate::layout;
use crate::model::{Base, Change, Stack, State};
use crate::submit::parse_meta;

/// A change as GitHub shows it: an open PR carrying gh-dags' metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteChange {
    pub name: String,
    pub pr: u64,
    /// The PR's base branch: the trunk, or another change.
    pub base: String,
    pub deps: BTreeSet<String>,
    /// The PR's head commit.
    pub head: String,
}

/// Where a PR sits in a GitHub stack, by PR number: (stack number, position
/// among the stack's open PRs).
pub type Placement = BTreeMap<u64, (u64, usize)>;

/// Positions in GitHub's open stacks. Merged PRs stay listed at a stack's
/// bottom; positions count only the open ones, like the layout does.
pub fn placement_of(stacks: &[RemoteStack]) -> Placement {
    let mut out = Placement::new();
    for s in stacks {
        let open = s.prs.iter().filter(|(_, open)| *open).map(|(n, _)| *n);
        for (i, n) in open.enumerate() {
            out.insert(n, (s.number, i));
        }
    }
    out
}

/// Find what to import on GitHub: the given PRs (or, with `all`, every open
/// PR gh-dags made), plus every PR they depend on or sit on.
pub fn discover(
    gh: &Gh,
    trunk: &str,
    seeds: &[u64],
    all: bool,
) -> Result<(Vec<RemoteChange>, Placement)> {
    let mut todo: Vec<u64> = if all { gh.open_prs()? } else { seeds.to_vec() };
    let mut seen = BTreeSet::new();
    let mut asked = BTreeSet::new();
    let mut found: BTreeMap<String, RemoteChange> = BTreeMap::new();
    while !todo.is_empty() {
        let batch: Vec<u64> = todo.drain(..).filter(|n| seen.insert(*n)).collect();
        let info = gh.prs(&batch)?;
        for n in batch {
            let Some(pr) = info.get(&n) else {
                bail!("#{n} doesn't exist");
            };
            let meta = parse_meta(&pr.body);
            let meta = match (pr.state.as_str(), meta) {
                ("OPEN", Some(m)) => m,
                // `--all` takes what's there and skips the rest.
                _ if all => continue,
                ("OPEN", None) => {
                    bail!("#{n} wasn't opened by gh-dags: its description has no gh-dags metadata")
                }
                (state, _) => bail!(
                    "#{n} is {}, so there's nothing to import",
                    state.to_lowercase()
                ),
            };
            if meta.change != pr.head {
                bail!(
                    "#{n}'s metadata is for {:?}, but its branch is {:?}",
                    meta.change,
                    pr.head
                );
            }
            // What it needs, and what it sits on, come along.
            for b in meta.deps.iter().chain([&pr.base]) {
                if b != trunk
                    && asked.insert(b.clone())
                    && let Some(p) = gh.open_pr_for(b)?
                {
                    todo.push(p);
                }
            }
            found.insert(
                pr.head.clone(),
                RemoteChange {
                    name: pr.head.clone(),
                    pr: n,
                    base: pr.base.clone(),
                    deps: meta.deps,
                    head: pr.head_sha.clone(),
                },
            );
        }
    }
    Ok((found.into_values().collect(), placement_of(&gh.stacks()?)))
}

/// The state these changes describe. Fork points are left to the caller,
/// which can see the commits.
pub fn rebuild(
    trunk: &str,
    remote: &str,
    changes: &[RemoteChange],
    placement: &Placement,
) -> Result<State> {
    let by_name: BTreeMap<&str, &RemoteChange> =
        changes.iter().map(|c| (c.name.as_str(), c)).collect();
    if by_name.len() != changes.len() {
        bail!("two PRs are for the same branch");
    }
    for c in changes {
        if c.name == trunk {
            bail!("#{} is for the trunk branch {trunk:?}", c.pr);
        }
        if c.base != trunk && !by_name.contains_key(c.base.as_str()) {
            bail!(
                "#{} ({}) is based on {:?}, which isn't a change being imported",
                c.pr,
                c.name,
                c.base
            );
        }
        if let Some(d) = c.deps.iter().find(|d| !by_name.contains_key(d.as_str())) {
            bail!(
                "#{} ({}) depends on {d:?}, which has no open PR",
                c.pr,
                c.name
            );
        }
    }

    // Older PRs were made first, so PR order is creation order.
    let mut order: Vec<&RemoteChange> = changes.iter().collect();
    order.sort_by_key(|c| c.pr);
    let mut st = State::new(trunk, remote);
    for (seq, c) in order.iter().enumerate() {
        st.changes.insert(
            c.name.clone(),
            Change {
                deps: c.deps.clone(),
                pr: Some(c.pr),
                pushed: Some(c.head.clone()),
                seq: seq as u64,
                ..Default::default()
            },
        );
    }
    st.next_seq = order.len() as u64;

    // Parents before children; among those ready, oldest first.
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    while placed.len() < order.len() {
        let Some(c) = order.iter().find(|c| {
            !placed.contains(c.name.as_str())
                && (c.base == trunk || placed.contains(c.base.as_str()))
        }) else {
            let stuck: Vec<String> = order
                .iter()
                .filter(|c| !placed.contains(c.name.as_str()))
                .map(|c| format!("#{}", c.pr))
                .collect();
            bail!("the bases of {} form a cycle", stuck.join(", "));
        };
        place(&mut st, c, placement);
        placed.insert(&c.name);
    }
    layout::normalize(&mut st);
    if let Err(e) = st.validate() {
        bail!("the PRs don't describe a valid layout: {e:#}");
    }
    Ok(st)
}

/// Put one change on its base: continuing the base's stack if GitHub has
/// them next to each other in a stack, else as a new stack (or spur).
fn place(st: &mut State, c: &RemoteChange, placement: &Placement) {
    let here = placement.get(&c.pr).copied();
    if c.base != st.trunk {
        let base = &st.changes[&c.base];
        let continues = match (here, base.pr.and_then(|n| placement.get(&n))) {
            (Some((s, i)), Some(&(t, j))) => s == t && i == j + 1,
            _ => false,
        };
        if continues && st.is_top(&c.base) {
            let (si, _) = st.locate(&c.base).unwrap();
            st.stacks[si].changes.push(c.name.clone());
            return;
        }
    }
    st.stacks.push(Stack {
        base: if c.base == st.trunk {
            Base::Trunk
        } else {
            Base::Change(c.base.clone())
        },
        changes: vec![c.name.clone()],
        github: here.map(|(s, _)| s),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `submit` shows GitHub for a state: a PR per submittable change,
    /// based on its parent, and a GitHub stack per layout stack of two or more.
    fn project(st: &State) -> (Vec<RemoteChange>, Placement) {
        let mut changes = Vec::new();
        let mut placement = Placement::new();
        for (si, s) in st.stacks.iter().enumerate() {
            if st.is_parked(&s.changes[0]) {
                continue;
            }
            for (i, n) in s.changes.iter().enumerate() {
                let pr = st.changes[n].seq + 1;
                changes.push(RemoteChange {
                    name: n.clone(),
                    pr,
                    base: match st.parent(n) {
                        Some(Base::Change(p)) => p,
                        _ => st.trunk.clone(),
                    },
                    deps: st.changes[n].deps.clone(),
                    head: format!("sha-{n}"),
                });
                if s.changes.len() >= 2 {
                    placement.insert(pr, (si as u64 + 1, i));
                }
            }
        }
        (changes, placement)
    }

    /// The state without its parked changes, which never get PRs.
    fn submitted(st: &State) -> State {
        let parked: BTreeSet<String> = st
            .changes
            .keys()
            .filter(|n| st.is_parked(n))
            .cloned()
            .collect();
        let mut st = st.clone();
        st.stacks.retain(|s| !parked.contains(&s.changes[0]));
        let keep: BTreeSet<String> = st.stacks.iter().flat_map(|s| s.changes.clone()).collect();
        st.changes.retain(|n, _| keep.contains(n));
        st
    }

    fn shape(st: &State) -> BTreeSet<(String, Vec<String>)> {
        st.stacks
            .iter()
            .map(|s| (format!("{:?}", s.base), s.changes.clone()))
            .collect()
    }

    fn check_roundtrip(st: &State) {
        let want = submitted(st);
        let (changes, placement) = project(st);
        let got = rebuild("main", "origin", &changes, &placement)
            .unwrap_or_else(|e| panic!("rebuild failed: {e:#}\n{want:?}"));
        assert_eq!(shape(&got), shape(&want), "layout differs");
        for (n, c) in &want.changes {
            assert_eq!(got.changes[n].deps, c.deps, "{n}'s deps differ");
            assert_eq!(got.parent(n), want.parent(n), "{n}'s parent differs");
        }
        let rank = |s: &State| -> Vec<String> {
            let mut v: Vec<(u64, String)> =
                s.changes.iter().map(|(n, c)| (c.seq, n.clone())).collect();
            v.sort();
            v.into_iter().map(|(_, n)| n).collect()
        };
        assert_eq!(rank(&got), rank(&want), "creation order differs");

        // Without GitHub's stacks, the grouping may differ, but every
        // change still sits on the same parent.
        let bare = rebuild("main", "origin", &changes, &Placement::new()).unwrap();
        for n in want.changes.keys() {
            assert_eq!(
                bare.parent(n),
                want.parent(n),
                "{n}'s parent differs without stacks"
            );
        }
    }

    #[test]
    fn rebuilds_every_small_layout_from_what_github_shows() {
        let (states, _) = crate::layout::exhaustive::explore(4, 6, false, check_roundtrip);
        assert!(states > 1000, "{states}");
    }

    fn change(name: &str, pr: u64, base: &str, deps: &[&str]) -> RemoteChange {
        RemoteChange {
            name: name.into(),
            pr,
            base: base.into(),
            deps: deps.iter().map(|d| d.to_string()).collect(),
            head: format!("sha-{name}"),
        }
    }

    #[test]
    fn refuses_what_isnt_a_whole_dag() {
        let err = |cs: &[RemoteChange]| {
            rebuild("main", "origin", cs, &Placement::new())
                .unwrap_err()
                .to_string()
        };
        assert!(
            err(&[change("a", 1, "x", &[])])
                .contains("based on \"x\", which isn't a change being imported")
        );
        assert!(
            err(&[change("a", 1, "main", &["gone"])])
                .contains("depends on \"gone\", which has no open PR")
        );
        assert!(
            err(&[change("a", 1, "b", &[]), change("b", 2, "a", &[])]).contains("form a cycle")
        );
        assert!(
            err(&[change("a", 1, "main", &[]), change("b", 2, "main", &["a"])])
                .contains("isn't stacked on it")
        );
    }

    #[test]
    fn placement_counts_only_open_prs() {
        let stacks = [RemoteStack {
            number: 7,
            base: "main".into(),
            prs: vec![(1, false), (2, false), (3, true), (4, true)],
        }];
        assert_eq!(
            placement_of(&stacks),
            Placement::from([(3, (7, 0)), (4, (7, 1))])
        );
    }

    #[test]
    fn a_change_inserted_below_an_older_one() {
        // `x` (PR 3) was linked in under `m` (PR 1): parents come first,
        // whatever their PR numbers.
        let cs = [
            change("m", 1, "x", &["x"]),
            change("n", 2, "m", &["m"]),
            change("x", 3, "main", &[]),
        ];
        let placement = Placement::from([(3, (9, 0)), (1, (9, 1)), (2, (9, 2))]);
        let st = rebuild("main", "origin", &cs, &placement).unwrap();
        assert_eq!(
            shape(&st),
            BTreeSet::from([("Trunk".into(), vec!["x".into(), "m".into(), "n".into()])])
        );
        assert_eq!(st.stacks[0].github, Some(9));
    }
}
