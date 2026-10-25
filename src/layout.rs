//! Layout operations (DESIGN.md §3–4). Each one runs on a copy of the state,
//! is normalized and validated against the invariant, and only then replaces
//! the original, so a rejected operation leaves nothing half-done.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use crate::model::{Base, Change, Stack, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanIn {
    /// Stack independent deps on each other (false deps, but submittable).
    Linearize,
    /// Base the change on a synthetic merge of its deps (local only).
    Park,
}

fn transact<T>(st: &mut State, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
    let mut work = st.clone();
    let out = f(&mut work)?;
    normalize(&mut work);
    work.validate()?;
    *st = work;
    Ok(out)
}

fn sum_base(mut members: Vec<String>) -> Base {
    match members.len() {
        0 => Base::Trunk,
        1 => Base::Change(members.pop().unwrap()),
        _ => Base::Sum(members),
    }
}

/// Track a new change that depends on `deps`, and place it in the layout.
pub fn add_change(st: &mut State, name: &str, deps: &[String], fan_in: FanIn) -> Result<()> {
    if st.has(name) {
        bail!("{name:?} is already tracked");
    }
    if name == st.trunk {
        bail!("{name:?} is the trunk");
    }
    for d in deps {
        st.change(d)?;
    }
    transact(st, |st| {
        let deps = st.reduce_deps(&deps.iter().cloned().collect());
        let tips = st.phys_maximal(&deps.iter().cloned().collect::<Vec<_>>());
        let onto = match tips.len() {
            0 => Base::Trunk,
            1 => Base::Change(tips[0].clone()),
            _ if fan_in == FanIn::Park => Base::Sum(tips),
            _ => {
                let mut deepest = tips[0].clone();
                for t in &tips[1..] {
                    deepest = ensure_comparable(st, &deepest, t)?;
                }
                Base::Change(deepest)
            }
        };
        let seq = st.next_seq;
        st.next_seq += 1;
        st.changes.insert(
            name.to_string(),
            Change {
                deps,
                seq,
                ..Default::default()
            },
        );
        place(st, vec![name.to_string()], onto, None);
        Ok(())
    })
}

/// Add a dependency edge, moving the layout minimally if `dep` isn't already
/// below `change`.
pub fn link(st: &mut State, change: &str, dep: &str) -> Result<()> {
    st.change(change)?;
    st.change(dep)?;
    if change == dep || st.logical_ancestors(dep).contains(change) {
        bail!("{dep:?} already depends on {change:?}; that would be a cycle");
    }
    transact(st, |st| {
        let mut deps = st.change(change)?.deps.clone();
        deps.insert(dep.to_string());
        st.change_mut(change)?.deps = st.reduce_deps(&deps);

        if st.phys_ancestors(change).contains(dep) {
            return Ok(());
        }
        if st.is_parked(change) || st.is_parked(dep) {
            bail!("linking parked changes isn't supported yet; use `move` to place them");
        }
        if st.subtree(change).contains(dep) {
            // `dep` sits above `change`. Pull what it needs down below `change`.
            let (si, ci) = st.locate(change).unwrap();
            if st.locate(dep).unwrap().0 != si {
                bail!("{dep:?} is on a spur above {change:?}; use `move` to rearrange first");
            }
            let mut need = st.logical_ancestors(dep);
            need.insert(dep.to_string());
            let changes = st.stacks[si].changes.clone();
            let (below, range) = changes.split_at(ci);
            let order = below
                .iter()
                .chain(range.iter().filter(|c| need.contains(*c)))
                .chain(range.iter().filter(|c| !need.contains(*c)))
                .cloned()
                .collect();
            reorder_stack(st, si, order);
            return Ok(());
        }
        // Not comparable: put the part of `change`'s path that branches off
        // from `dep`'s path on top of `dep`.
        let pc = st.path(change);
        let pd = st.path(dep);
        let k = common_prefix(&pc, &pd);
        reparent(st, &pc[k].clone(), Base::Change(dep.to_string()))
    })
}

/// Remove a dependency edge. The layout stays put; `dep` may become a false
/// dependency.
pub fn unlink(st: &mut State, change: &str, dep: &str) -> Result<()> {
    transact(st, |st| {
        if !st.change_mut(change)?.deps.remove(dep) {
            bail!("{change:?} doesn't depend on {dep:?}");
        }
        Ok(())
    })
}

/// Re-parent `change` (and everything stacked on it) onto `onto`.
pub fn move_onto(st: &mut State, change: &str, onto: Base) -> Result<()> {
    st.change(change)?;
    transact(st, |st| reparent(st, change, onto)).context("move rejected")
}

/// Put `change` and its logical ancestors at the bottom of one trunk stack,
/// ready for a prefix merge. Returns whether the layout changed.
///
/// GitHub merges a prefix of one stack, so `change`'s whole root path (which
/// may run through spurs) becomes a single trunk stack: what it needs, then
/// the ordering-only rest, then whatever sat above `change`. Side branches
/// stay on their node; spurs are re-anchored if they'd lose a dependency.
pub fn hoist(st: &mut State, change: &str) -> Result<bool> {
    st.change(change)?;
    if st.is_parked(change) {
        bail!("{change:?} is parked; it can land once all but one of its sum has");
    }
    transact(st, |st| {
        let before = layout_key(st);
        let path = st.path(change);
        let mut need = st.logical_ancestors(change);
        need.insert(change.to_string());

        // The path covers a prefix of each stack it runs through. Replace the
        // trunk stack it starts in with the new chain; whatever a stack had
        // beyond the path stays on the path's last node there (above
        // `change`, that's the continuation of the chain).
        let mut chain: Vec<String> = path
            .iter()
            .filter(|c| need.contains(*c))
            .chain(path.iter().filter(|c| !need.contains(*c)))
            .cloned()
            .collect();
        let mut stacks = Vec::new();
        let mut root = 0;
        let mut sides = Vec::new();
        for s in std::mem::take(&mut st.stacks) {
            let k = s.changes.iter().take_while(|c| path.contains(c)).count();
            if k == 0 {
                stacks.push(s);
                continue;
            }
            let last = s.changes[k - 1].clone();
            let rest = s.changes[k..].to_vec();
            if s.base == Base::Trunk {
                root = stacks.len();
                stacks.push(Stack {
                    base: Base::Trunk,
                    changes: vec![],
                    github: s.github,
                });
            }
            if last == change {
                chain.extend(rest);
            } else if !rest.is_empty() {
                sides.push(Stack {
                    base: Base::Change(last),
                    changes: rest,
                    github: None,
                });
            }
        }
        stacks[root].changes = chain;
        stacks.splice(root + 1..root + 1, sides);
        st.stacks = stacks;
        reanchor(st, root);
        normalize(st);
        Ok(layout_key(st) != before)
    })
}

/// The layout without stack order or GitHub numbers, to tell real changes.
fn layout_key(st: &State) -> BTreeSet<(String, Vec<String>)> {
    st.stacks
        .iter()
        .map(|s| (format!("{:?}", s.base), s.changes.clone()))
        .collect()
}

/// A change merged into trunk: drop it, re-root what sat on it, shrink sums.
pub fn absorb_merged(st: &mut State, change: &str) -> Result<()> {
    transact(st, |st| {
        let (si, pos) = st
            .locate(change)
            .with_context(|| format!("{change:?} is not a tracked change"))?;
        if pos != 0 || st.stacks[si].base != Base::Trunk {
            bail!("{change:?} merged, but changes below it haven't");
        }
        st.stacks[si].changes.remove(0);
        for s in &mut st.stacks {
            s.base = match std::mem::replace(&mut s.base, Base::Trunk) {
                Base::Change(b) if b == change => Base::Trunk,
                Base::Sum(ms) => sum_base(ms.into_iter().filter(|m| m != change).collect()),
                other => other,
            };
        }
        st.changes.remove(change);
        for c in st.changes.values_mut() {
            c.deps.remove(change);
        }
        Ok(())
    })
}

/// Stop tracking a change that nothing sits on or depends on.
pub fn untrack(st: &mut State, change: &str) -> Result<()> {
    st.change(change)?;
    if let Some(child) = st.children(change).first() {
        bail!("{child:?} is stacked on {change:?}; move or untrack it first");
    }
    if let Some((d, _)) = st.changes.iter().find(|(_, c)| c.deps.contains(change)) {
        bail!("{d:?} depends on {change:?}; unlink it first");
    }
    transact(st, |st| {
        let (si, _) = st.locate(change).unwrap();
        st.stacks[si].changes.retain(|c| c != change);
        st.changes.remove(change);
        Ok(())
    })
}

/// Make one of `x`, `y` a physical ancestor of the other with a single
/// re-parent, choosing the move that creates fewer false deps. Returns the
/// one that ends up deeper.
fn ensure_comparable(st: &mut State, x: &str, y: &str) -> Result<String> {
    if st.phys_ancestors(y).contains(x) {
        return Ok(y.to_string());
    }
    if st.phys_ancestors(x).contains(y) {
        return Ok(x.to_string());
    }
    for n in [x, y] {
        if st.is_parked(n) {
            bail!("{n:?} is parked, so it can't be linearized with other deps; use --park");
        }
    }
    let px = st.path(x);
    let py = st.path(y);
    let k = common_prefix(&px, &py);
    let (bx, by) = (px[k].clone(), py[k].clone());
    // Each node in the moved subtree gains the segment it lands on as false deps.
    let cost_move_y = st.subtree(&by).len() * (px.len() - k);
    let cost_move_x = st.subtree(&bx).len() * (py.len() - k);
    let move_y = match cost_move_y.cmp(&cost_move_x) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        // Tie: newer work goes on top.
        std::cmp::Ordering::Equal => st.change(&by)?.seq >= st.change(&bx)?.seq,
    };
    if move_y {
        reparent(st, &by, Base::Change(x.to_string()))?;
        Ok(y.to_string())
    } else {
        reparent(st, &bx, Base::Change(y.to_string()))?;
        Ok(x.to_string())
    }
}

fn common_prefix(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Detach `node` and everything above it in its stack (spurs follow, since
/// they reference changes by name) and put it on `onto`.
fn reparent(st: &mut State, node: &str, onto: Base) -> Result<()> {
    let members: Vec<String> = match &onto {
        Base::Trunk => vec![],
        Base::Change(x) => vec![x.clone()],
        Base::Sum(ms) => ms.clone(),
    };
    let sub = st.subtree(node);
    if let Some(m) = members.iter().find(|m| sub.contains(*m)) {
        bail!("can't put {node:?} on {m:?}: {m:?} is stacked on {node:?}");
    }
    let (si, pos) = st
        .locate(node)
        .with_context(|| format!("{node:?} is not a tracked change"))?;
    let tail = st.stacks[si].changes.split_off(pos);
    let github = if pos == 0 {
        st.stacks.remove(si).github
    } else {
        None
    };
    place(st, tail, onto, github);
    Ok(())
}

/// Put a chain of changes on `onto`: appended when `onto` is a stack top,
/// otherwise as a new stack (a spur, a parked stack, or a trunk stack).
fn place(st: &mut State, chain: Vec<String>, onto: Base, github: Option<u64>) {
    if let Base::Change(x) = &onto
        && st.is_top(x)
    {
        let (si, _) = st.locate(x).unwrap();
        st.stacks[si].changes.extend(chain);
        return;
    }
    st.stacks.push(Stack {
        base: onto,
        changes: chain,
        github,
    });
}

/// Apply a new order to one stack, then re-anchor stacks hanging off it onto
/// the highest node they actually need, so none of them loses a dependency.
fn reorder_stack(st: &mut State, si: usize, order: Vec<String>) {
    st.stacks[si].changes = order;
    reanchor(st, si);
}

/// After stack `si` was reordered, move each stack anchored on it up to the
/// highest node it actually needs.
fn reanchor(st: &mut State, si: usize) {
    let pos: BTreeMap<String, usize> = st.stacks[si]
        .changes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.clone(), i))
        .collect();
    for ti in 0..st.stacks.len() {
        if ti == si {
            continue;
        }
        let anchors = match &st.stacks[ti].base {
            Base::Trunk => continue,
            Base::Change(a) => vec![a.clone()],
            Base::Sum(ms) => ms.clone(),
        };
        if !anchors.iter().any(|a| pos.contains_key(a)) {
            continue;
        }
        let mut needed = BTreeSet::new();
        for n in st.subtree(&st.stacks[ti].changes[0]) {
            needed.extend(st.logical_ancestors(&n));
        }
        let anchors: Vec<String> = anchors
            .into_iter()
            .map(|a| {
                if !pos.contains_key(&a) {
                    return a;
                }
                needed
                    .iter()
                    .chain(std::iter::once(&a))
                    .filter(|n| pos.contains_key(*n))
                    .max_by_key(|n| pos[*n])
                    .unwrap()
                    .clone()
            })
            .collect();
        st.stacks[ti].base = match st.stacks[ti].base {
            Base::Sum(_) => sum_base(anchors),
            _ => Base::Change(anchors[0].clone()),
        };
    }
}

/// Canonical form: sums keep only maximal members (collapsing to a plain base
/// when one is left), and a lone spur on a stack top folds into that stack.
fn normalize(st: &mut State) {
    st.stacks.retain(|s| !s.changes.is_empty());
    for i in 0..st.stacks.len() {
        if let Base::Sum(ms) = &st.stacks[i].base {
            let ms = st.phys_maximal(ms);
            st.stacks[i].base = sum_base(ms);
        }
    }
    'fold: loop {
        for ti in 0..st.stacks.len() {
            let Base::Change(a) = &st.stacks[ti].base else {
                continue;
            };
            let on_a = st
                .stacks
                .iter()
                .filter(|s| matches!(&s.base, Base::Change(b) if b == a))
                .count();
            if !st.is_top(a) || on_a != 1 {
                continue;
            }
            let (si, _) = st.locate(a).unwrap();
            let moved = st.stacks.remove(ti);
            let si = if si > ti { si - 1 } else { si };
            st.stacks[si].changes.extend(moved.changes);
            continue 'fold;
        }
        break;
    }
}

pub fn describe_base(st: &State, base: &Base) -> String {
    match base {
        Base::Trunk => st.trunk.clone(),
        Base::Change(c) => c.clone(),
        Base::Sum(ms) => ms.join(" ⊕ "),
    }
}

/// Human-readable lines for every change whose physical parent moved.
pub fn describe_moves(before: &State, after: &State) -> Vec<String> {
    let mut out = Vec::new();
    for name in after.changes.keys() {
        let now = after.parent(name);
        let was = before.parent(name);
        if now == was {
            continue;
        }
        let now = now.map(|b| describe_base(after, &b)).unwrap_or_default();
        let mut line = match was {
            None => format!("{name} on {now}"),
            Some(was) => format!("{name} now on {now} (was {})", describe_base(before, &was)),
        };
        if after.ordering_only_parent(name).is_some() {
            line.push_str(" · ordering only");
        }
        out.push(line);
    }
    for name in before.changes.keys() {
        if !after.has(name) {
            out.push(format!("{name} removed"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> State {
        State::new("main", "origin")
    }

    fn add(st: &mut State, name: &str, deps: &[&str]) {
        let deps: Vec<String> = deps.iter().map(|s| s.to_string()).collect();
        add_change(st, name, &deps, FanIn::Linearize).unwrap();
    }

    fn park(st: &mut State, name: &str, deps: &[&str]) {
        let deps: Vec<String> = deps.iter().map(|s| s.to_string()).collect();
        add_change(st, name, &deps, FanIn::Park).unwrap();
    }

    /// Stacks as "base: a b c" strings, for compact assertions.
    fn shape(st: &State) -> Vec<String> {
        st.stacks
            .iter()
            .map(|s| format!("{}: {}", describe_base(st, &s.base), s.changes.join(" ")))
            .collect()
    }

    #[test]
    fn chains_append_and_fan_out_makes_spurs() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &["a"]);
        add(&mut s, "c", &["a"]);
        assert_eq!(shape(&s), ["main: a b", "a: c"]);
        assert!(s.false_deps("c").is_empty());
        assert!(s.false_deps("b").is_empty());
    }

    #[test]
    fn fan_in_linearizes_newer_on_top() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &[]);
        add(&mut s, "c", &["a", "b"]);
        assert_eq!(shape(&s), ["main: a b c"]);
        assert_eq!(s.ordering_only_parent("b").as_deref(), Some("a"));
        assert!(s.false_deps("c").is_empty());
    }

    #[test]
    fn fan_in_moves_the_cheaper_side() {
        // a has two things on it; moving a would give both a false dep.
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "a1", &["a"]);
        add(&mut s, "a2", &["a"]);
        add(&mut s, "b", &[]);
        add(&mut s, "c", &["a", "b"]);
        assert_eq!(shape(&s), ["main: a a1", "a: a2", "a: b c"]);
        assert_eq!(s.false_deps("a2"), BTreeSet::new());
    }

    #[test]
    fn redundant_deps_are_reduced() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &["a"]);
        add(&mut s, "c", &["a", "b"]);
        assert_eq!(
            s.change("c").unwrap().deps,
            BTreeSet::from(["b".to_string()])
        );
    }

    #[test]
    fn park_bases_on_a_sum() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &[]);
        park(&mut s, "c", &["a", "b"]);
        add(&mut s, "d", &["c"]);
        assert_eq!(shape(&s), ["main: a", "main: b", "a ⊕ b: c d"]);
        assert!(s.is_parked("d"));
        assert!(!s.is_parked("a"));
    }

    #[test]
    fn hoist_moves_ancestors_to_the_bottom() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &[]);
        add(&mut s, "c", &["a", "b"]);
        add(&mut s, "d", &["c"]);
        unlink(&mut s, "d", "c").unwrap();
        add(&mut s, "e", &["b", "d"]);
        // a b c d e, where e needs b and d only.
        assert_eq!(shape(&s), ["main: a b c d e"]);
        assert!(hoist(&mut s, "e").unwrap());
        assert_eq!(shape(&s), ["main: b d e a c"]);
        assert!(!hoist(&mut s, "e").unwrap(), "hoist is idempotent");
    }

    #[test]
    fn hoist_reanchors_spurs_that_would_lose_a_dep() {
        let mut s = st();
        add(&mut s, "p", &[]);
        add(&mut s, "q", &["p"]);
        add(&mut s, "b", &["q"]);
        unlink(&mut s, "b", "q").unwrap();
        add(&mut s, "y", &["b"]);
        add(&mut s, "x", &["b", "q"]);
        assert_eq!(shape(&s), ["main: p q b y", "b: x"]);
        assert!(hoist(&mut s, "b").unwrap());
        assert_eq!(shape(&s), ["main: b p q y", "q: x"]);
    }

    #[test]
    fn hoist_pulls_an_ordering_only_spur_down_to_trunk() {
        // theme was linearized onto auth (which has a side branch), so it sits
        // on a spur; landing it must not require landing auth.
        let mut s = st();
        add(&mut s, "auth", &[]);
        add(&mut s, "tests", &["auth"]);
        add(&mut s, "theme", &[]);
        add(&mut s, "login", &["auth", "theme"]);
        assert_eq!(shape(&s), ["main: auth tests", "auth: theme login"]);
        assert!(hoist(&mut s, "theme").unwrap());
        assert_eq!(shape(&s), ["main: theme auth login", "auth: tests"]);
        assert!(s.false_deps("theme").is_empty());
        assert!(!hoist(&mut s, "theme").unwrap());
    }

    #[test]
    fn hoist_joins_a_spur_into_its_dependency_stack() {
        // x really needs a, but sits on a spur; GitHub can only merge a prefix
        // of one stack, so x has to join a's stack (a2 becomes the spur).
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "a2", &["a"]);
        add(&mut s, "x", &["a"]);
        assert_eq!(shape(&s), ["main: a a2", "a: x"]);
        assert!(hoist(&mut s, "x").unwrap());
        assert_eq!(shape(&s), ["main: a x", "a: a2"]);
        assert_eq!(
            s.parent("a2"),
            Some(Base::Change("a".into())),
            "nothing moved physically"
        );
        assert!(!hoist(&mut s, "x").unwrap());
    }

    #[test]
    fn absorb_reroots_children_and_unparks() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &["a"]);
        add(&mut s, "c", &["a"]);
        add(&mut s, "z", &[]);
        park(&mut s, "p", &["a", "z"]);
        assert_eq!(shape(&s), ["main: a b", "a: c", "main: z", "a ⊕ z: p"]);
        absorb_merged(&mut s, "a").unwrap();
        assert_eq!(shape(&s), ["main: b", "main: c", "main: z p"]);
        assert!(s.change("p").unwrap().deps.contains("z"));
        assert!(!s.change("b").unwrap().deps.contains("a"));
    }

    #[test]
    fn absorb_requires_a_merged_prefix() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &["a"]);
        assert!(absorb_merged(&mut s, "b").is_err());
        assert_eq!(shape(&s), ["main: a b"]);
    }

    #[test]
    fn move_is_validated() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &["a"]);
        let before = s.clone();
        assert!(move_onto(&mut s, "b", Base::Trunk).is_err());
        assert_eq!(s, before, "rejected ops change nothing");
        assert!(move_onto(&mut s, "a", Base::Change("b".into())).is_err());
        add(&mut s, "c", &[]);
        move_onto(&mut s, "c", Base::Change("a".into())).unwrap();
        assert_eq!(shape(&s), ["main: a b", "a: c"]);
    }

    #[test]
    fn link_across_stacks_and_within_a_stack() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &[]);
        link(&mut s, "b", "a").unwrap();
        assert_eq!(shape(&s), ["main: a b"]);

        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &["a"]);
        add(&mut s, "c", &["b"]);
        unlink(&mut s, "c", "b").unwrap();
        link(&mut s, "a", "c").unwrap();
        assert_eq!(shape(&s), ["main: c a b"]);

        assert!(
            link(&mut s, "c", "b").is_err(),
            "b → a → c, so c → b is a cycle"
        );
    }

    #[test]
    fn untrack_only_leaves() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &["a"]);
        assert!(untrack(&mut s, "a").is_err());
        untrack(&mut s, "b").unwrap();
        untrack(&mut s, "a").unwrap();
        assert!(s.stacks.is_empty());
    }

    #[test]
    fn describe_moves_reports_reparents() {
        let mut s = st();
        add(&mut s, "a", &[]);
        add(&mut s, "b", &[]);
        let before = s.clone();
        add(&mut s, "c", &["a", "b"]);
        assert_eq!(
            describe_moves(&before, &s),
            ["b now on a (was main) · ordering only", "c on b"]
        );
    }

    /// Random op sequences never panic and always leave a valid state.
    #[test]
    fn random_ops_keep_the_invariant() {
        let mut seed: u64 = 0x5eed;
        let mut rand = |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n.max(1)
        };
        let (mut tried, mut applied) = (0, 0);
        for _round in 0..200 {
            let mut s = st();
            for step in 0..25 {
                let before = s.clone();
                let names: Vec<String> = s.changes.keys().cloned().collect();
                let pick = |r: usize| names.get(r).cloned().unwrap_or_default();
                let result = match rand(7) {
                    0 | 1 => {
                        let deps: Vec<String> = (0..rand(3))
                            .map(|_| pick(rand(names.len())))
                            .filter(|d| !d.is_empty())
                            .collect();
                        let fan = if rand(4) == 0 {
                            FanIn::Park
                        } else {
                            FanIn::Linearize
                        };
                        add_change(&mut s, &format!("n{step}"), &deps, fan)
                    }
                    2 => link(&mut s, &pick(rand(names.len())), &pick(rand(names.len()))),
                    3 => {
                        let c = pick(rand(names.len()));
                        let d = s
                            .changes
                            .get(&c)
                            .and_then(|c| c.deps.iter().next().cloned())
                            .unwrap_or_default();
                        unlink(&mut s, &c, &d)
                    }
                    4 => hoist(&mut s, &pick(rand(names.len()))).map(|_| ()),
                    5 => {
                        let bottom = s
                            .stacks
                            .iter()
                            .find(|st| st.base == Base::Trunk)
                            .map(|st| st.changes[0].clone());
                        absorb_merged(&mut s, &bottom.unwrap_or_default())
                    }
                    _ => {
                        let onto = if rand(3) == 0 {
                            Base::Trunk
                        } else {
                            Base::Change(pick(rand(names.len())))
                        };
                        move_onto(&mut s, &pick(rand(names.len())), onto)
                    }
                };
                tried += 1;
                match result {
                    Ok(()) => applied += 1,
                    Err(_) => assert_eq!(s, before, "a rejected op must change nothing"),
                }
                s.validate().unwrap();
            }
        }
        assert!(
            applied * 3 > tried,
            "only {applied} of {tried} random ops applied"
        );
    }
}
