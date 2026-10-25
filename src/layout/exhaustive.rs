//! Exhaustive bounded checking of the layout operations: every state
//! reachable from empty with up to `N` changes and `DEPTH` operations, every
//! operation applied to each, and each transition checked against precise
//! per-operation properties (not just the invariant, which `transact` already
//! enforces). States are deduplicated up to creation order, so the search
//! covers the space rather than paths through it.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use super::*;

#[derive(Debug, Clone)]
enum Op {
    Add(String, Vec<String>, FanIn),
    Link(String, String),
    Unlink(String, String),
    Move(String, Base),
    Hoist(String),
    Absorb(String),
    Untrack(String),
}

const POOL: [&str; 5] = ["a", "b", "c", "d", "e"];

fn subsets(items: &[String]) -> Vec<Vec<String>> {
    (0..1u32 << items.len())
        .map(|mask| {
            items
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, s)| s.clone())
                .collect()
        })
        .collect()
}

/// Every operation worth trying in `st`. New changes take the first free
/// name: names are interchangeable, so this loses nothing and avoids
/// exploring permutations.
fn ops(st: &State, max: usize, trees_only: bool) -> Vec<Op> {
    let names: Vec<String> = st.changes.keys().cloned().collect();
    let mut out = Vec::new();
    if names.len() < max
        && let Some(new) = POOL.iter().find(|n| !st.has(n))
    {
        for deps in subsets(&names) {
            if trees_only && deps.len() > 1 {
                continue;
            }
            if deps.len() >= 2 {
                out.push(Op::Add(new.to_string(), deps.clone(), FanIn::Park));
            }
            out.push(Op::Add(new.to_string(), deps, FanIn::Linearize));
        }
    }
    for s in &st.stacks {
        if s.base == Base::Trunk {
            out.push(Op::Absorb(s.changes[0].clone()));
        }
    }
    for c in &names {
        out.push(Op::Untrack(c.clone()));
        if trees_only {
            continue;
        }
        out.push(Op::Hoist(c.clone()));
        out.push(Op::Move(c.clone(), Base::Trunk));
        for d in &names {
            if c != d {
                out.push(Op::Link(c.clone(), d.clone()));
                out.push(Op::Move(c.clone(), Base::Change(d.clone())));
            }
        }
        for d in &st.changes[c].deps {
            out.push(Op::Unlink(c.clone(), d.clone()));
        }
    }
    out
}

fn apply(st: &mut State, op: &Op) -> Result<bool> {
    match op {
        Op::Add(n, d, f) => add_change(st, n, d, *f).map(|_| true),
        Op::Link(c, d) => link(st, c, d).map(|_| true),
        Op::Unlink(c, d) => unlink(st, c, d).map(|_| true),
        Op::Move(c, b) => move_onto(st, c, b.clone()).map(|_| true),
        Op::Hoist(c) => hoist(st, c),
        Op::Absorb(c) => absorb_merged(st, c).map(|_| true),
        Op::Untrack(c) => untrack(st, c).map(|_| true),
    }
}

/// The state up to creation order: `seq` values only matter relative to
/// each other.
fn key(st: &State) -> String {
    let mut st = st.clone();
    let mut seqs: Vec<u64> = st.changes.values().map(|c| c.seq).collect();
    seqs.sort();
    for c in st.changes.values_mut() {
        c.seq = seqs.binary_search(&c.seq).unwrap() as u64;
    }
    st.next_seq = 0;
    serde_json::to_string(&st).unwrap()
}

fn deps_of(st: &State) -> BTreeMap<String, BTreeSet<String>> {
    st.changes
        .iter()
        .map(|(n, c)| (n.clone(), c.deps.clone()))
        .collect()
}

fn parents(st: &State) -> BTreeMap<String, Option<Base>> {
    st.changes
        .keys()
        .map(|n| (n.clone(), st.parent(n)))
        .collect()
}

fn ancestry(st: &State) -> BTreeMap<String, BTreeSet<String>> {
    st.changes
        .keys()
        .map(|n| (n.clone(), st.phys_ancestors(n)))
        .collect()
}

/// Deps of every change except `except`, before and after, must match.
fn other_deps_unchanged(b: &State, a: &State, except: &[&str]) {
    for (n, d) in deps_of(b) {
        if !except.contains(&n.as_str()) && a.has(&n) {
            assert_eq!(a.changes[&n].deps, d, "deps of {n} changed");
        }
    }
}

fn check(b: &State, op: &Op, result: &Result<bool>, a: &State) {
    let ctx = || format!("\nop: {op:?}\nbefore: {}\nafter: {}", key(b), key(a));
    let Ok(changed) = result else {
        assert_eq!(a, b, "a rejected op changed the state{}", ctx());
        // Completeness: these must never be rejected.
        match op {
            Op::Add(_, deps, f) => {
                let tips = b.phys_maximal(
                    &b.reduce_deps(&deps.iter().cloned().collect())
                        .into_iter()
                        .collect::<Vec<_>>(),
                );
                let parked = tips.iter().any(|t| b.is_parked(t));
                assert!(
                    *f == FanIn::Park || tips.len() < 2 || parked,
                    "add rejected: {:?}{}",
                    result.as_ref().unwrap_err(),
                    ctx()
                );
            }
            Op::Hoist(c) => assert!(
                b.is_parked(c),
                "hoist rejected: {:?}{}",
                result.as_ref().unwrap_err(),
                ctx()
            ),
            Op::Absorb(_) => panic!("absorbing a trunk stack's bottom was rejected{}", ctx()),
            Op::Unlink(..) => panic!("unlinking an existing dep was rejected{}", ctx()),
            Op::Link(c, d) if b.phys_ancestors(c).contains(d) => {
                panic!(
                    "linking to an existing physical ancestor was rejected{}",
                    ctx()
                )
            }
            Op::Untrack(c) => assert!(
                !b.children(c).is_empty() || b.changes.values().any(|x| x.deps.contains(c)),
                "untracking a leaf was rejected{}",
                ctx()
            ),
            _ => {}
        }
        return;
    };

    // Always: valid, canonical, and a topological order that respects parents.
    a.validate()
        .unwrap_or_else(|e| panic!("invalid: {e}{}", ctx()));
    let mut norm = a.clone();
    normalize(&mut norm);
    assert_eq!(&norm, a, "not normalized{}", ctx());
    let order = a.topo_order().unwrap();
    let pos: BTreeMap<&String, usize> = order.iter().enumerate().map(|(i, n)| (n, i)).collect();
    for n in a.changes.keys() {
        for p in a.phys_ancestors(n) {
            assert!(
                pos[&p] < pos[n],
                "{p} isn't before {n} in topo order{}",
                ctx()
            );
        }
    }

    match op {
        Op::Add(x, deps, fan) => {
            let reduced = b.reduce_deps(&deps.iter().cloned().collect());
            assert_eq!(a.changes[x].deps, reduced, "{}", ctx());
            other_deps_unchanged(b, a, &[]);
            let (ab, aa) = (ancestry(b), ancestry(a));
            for (n, anc) in &ab {
                assert!(anc.is_subset(&aa[n]), "{n} lost an ancestor{}", ctx());
            }
            let tips = b.phys_maximal(&reduced.iter().cloned().collect::<Vec<_>>());
            if tips.len() <= 1 {
                for (n, p) in parents(b) {
                    assert_eq!(
                        a.parent(&n),
                        p,
                        "{n} moved though no fan-in was needed{}",
                        ctx()
                    );
                }
                if let Some(d) = tips.first() {
                    assert!(
                        a.false_deps(x).is_subset(&a.false_deps(d)),
                        "{x} got new false deps{}",
                        ctx()
                    );
                } else {
                    assert!(a.false_deps(x).is_empty(), "{}", ctx());
                }
            } else if *fan == FanIn::Park {
                assert_eq!(a.parent(x), Some(Base::Sum(tips.clone())), "{}", ctx());
                for (n, p) in parents(b) {
                    assert_eq!(a.parent(&n), p, "parking moved {n}{}", ctx());
                }
            }
        }
        Op::Link(c, d) => {
            let mut want = b.changes[c].deps.clone();
            want.insert(d.clone());
            assert_eq!(a.changes[c].deps, b.reduce_deps(&want), "{}", ctx());
            other_deps_unchanged(b, a, &[c]);
            assert!(a.phys_ancestors(c).contains(d), "{}", ctx());
        }
        Op::Unlink(c, d) => {
            assert_eq!(a.stacks, b.stacks, "unlink moved the layout{}", ctx());
            let mut want = b.changes[c].deps.clone();
            want.remove(d);
            assert_eq!(a.changes[c].deps, want, "{}", ctx());
            other_deps_unchanged(b, a, &[c]);
        }
        Op::Move(c, onto) => {
            other_deps_unchanged(b, a, &[]);
            assert_eq!(a.parent(c).as_ref(), Some(onto), "{}", ctx());
        }
        Op::Hoist(x) => {
            matches_proven_model(b, x, a).unwrap_or_else(|e| panic!("{e}{}", ctx()));
            other_deps_unchanged(b, a, &[]);
            assert_eq!(*changed, layout_key(a) != layout_key(b), "{}", ctx());
            assert_eq!(
                a.phys_ancestors(x),
                a.logical_ancestors(x),
                "{x} still has false deps{}",
                ctx()
            );
            let (si, p) = a.locate(x).unwrap();
            assert_eq!(a.stacks[si].base, Base::Trunk, "{}", ctx());
            assert_eq!(
                p,
                a.logical_ancestors(x).len(),
                "{x}'s needs aren't a prefix{}",
                ctx()
            );
            let mut again = a.clone();
            assert!(
                !hoist(&mut again, x).unwrap(),
                "hoist isn't idempotent{}",
                ctx()
            );
        }
        Op::Absorb(m) => {
            assert!(!a.has(m), "{}", ctx());
            let ab = ancestry(b);
            for (n, d) in deps_of(a) {
                let mut want = b.changes[&n].deps.clone();
                want.remove(m);
                assert_eq!(d, want, "{}", ctx());
                let mut anc = ab[&n].clone();
                anc.remove(m);
                assert_eq!(
                    a.phys_ancestors(&n),
                    anc,
                    "{n}'s ancestry changed beyond losing {m}{}",
                    ctx()
                );
            }
        }
        Op::Untrack(c) => {
            assert!(!a.has(c), "{}", ctx());
            other_deps_unchanged(b, a, &[]);
            for (n, p) in parents(a) {
                assert_eq!(p, b.parent(&n), "untracking {c} moved {n}{}", ctx());
            }
        }
    }
}

/// The hoist that `proofs/GhDags/Hoist.lean` proves correct, checked against
/// what the code did. With `P` the root path of `x` and
/// `C = P.filter(need) ++ P.filter(!need)`:
///
/// * path nodes sit on their predecessor in `C` (the first on trunk);
/// * `x`'s stack successor sits on the last node of `C`;
/// * each other branch hanging off `P` is re-anchored onto a node of `P` at or
///   after (in `C`) everything that branch needs from `P`;
/// * every other change keeps its parent.
///
/// Where this holds, `newPar_inv`, `newPar_exact` and `newPar_acyclic` apply
/// to the code's actual result. Sums are outside the model, so changes
/// sitting on one are skipped.
fn matches_proven_model(b: &State, x: &str, a: &State) -> std::result::Result<(), String> {
    let path = b.path(x);
    let need: BTreeSet<String> = b
        .logical_ancestors(x)
        .into_iter()
        .chain([x.to_string()])
        .collect();
    let chain: Vec<String> = path
        .iter()
        .filter(|v| need.contains(*v))
        .chain(path.iter().filter(|v| !need.contains(*v)))
        .cloned()
        .collect();
    let pos = |v: &str| chain.iter().position(|c| c == v);
    let (si, p) = b.locate(x).unwrap();
    let cont = b.stacks[si].changes.get(p + 1).cloned();
    for v in b.changes.keys() {
        let (before, after) = (b.parent(v).unwrap(), a.parent(v).unwrap());
        if matches!(before, Base::Sum(_)) || matches!(after, Base::Sum(_)) {
            continue;
        }
        let expected = if let Some(i) = pos(v) {
            Some(if i == 0 {
                Base::Trunk
            } else {
                Base::Change(chain[i - 1].clone())
            })
        } else if cont.as_deref() == Some(v.as_str()) {
            Some(Base::Change(chain.last().unwrap().clone()))
        } else if matches!(&before, Base::Change(q) if path.contains(q)) {
            // A side root: any anchor satisfying the proven rule is fine.
            let Base::Change(anchor) = &after else {
                return Err(format!(
                    "side root {v} isn't anchored on a change: {after:?}"
                ));
            };
            let Some(at) = pos(anchor) else {
                return Err(format!(
                    "side root {v} is anchored off the path, on {anchor}"
                ));
            };
            let needs: BTreeSet<String> = b
                .subtree(v)
                .iter()
                .flat_map(|w| b.logical_ancestors(w))
                .collect();
            for d in needs.iter().filter(|d| path.contains(*d)) {
                if pos(d).unwrap() > at {
                    return Err(format!(
                        "side root {v} is anchored on {anchor}, below {d}, which it needs"
                    ));
                }
            }
            None
        } else {
            Some(before.clone())
        };
        if let Some(e) = expected
            && e != after
        {
            return Err(format!(
                "{v} sits on {after:?}, but the proven model puts it on {e:?}"
            ));
        }
    }
    Ok(())
}

/// Breadth-first over distinct states; returns (states, transitions).
fn explore(
    max: usize,
    depth: usize,
    trees_only: bool,
    mut each: impl FnMut(&State),
) -> (usize, usize) {
    let start = State::new("main", "origin");
    let mut seen = HashSet::from([key(&start)]);
    let mut queue = VecDeque::from([(start, 0)]);
    let mut transitions = 0;
    while let Some((st, d)) = queue.pop_front() {
        each(&st);
        if d == depth {
            continue;
        }
        for op in ops(&st, max, trees_only) {
            let mut next = st.clone();
            let result = apply(&mut next, &op);
            check(&st, &op, &result, &next);
            transitions += 1;
            if result.is_ok() && seen.insert(key(&next)) {
                queue.push_back((next, d + 1));
            }
        }
    }
    (seen.len(), transitions)
}

#[test]
fn every_operation_on_every_small_state() {
    let (states, transitions) = explore(3, 8, false, |_| {});
    eprintln!("checked {transitions} transitions over {states} states (≤3 changes)");
    assert!(
        states > 500,
        "the search should cover a real space, got {states}"
    );
}

/// Some bugs need four changes to show (a spur re-anchored by `land` needs a
/// stack of three under it), so the default suite goes there too, shallower.
#[test]
fn every_operation_with_four_changes_shallow() {
    let (states, transitions) = explore(4, 6, false, |_| {});
    eprintln!("checked {transitions} transitions over {states} states (≤4 changes, depth 6)");
}

#[test]
fn trees_never_get_false_deps() {
    let (states, _) = explore(5, 9, true, |st| {
        for n in st.changes.keys() {
            assert!(
                st.false_deps(n).is_empty(),
                "{n} has false deps in a tree: {}",
                key(st)
            );
        }
    });
    assert!(states > 50, "{states}");
}

/// The big one: ≤4 changes, deeper. Run with
/// `cargo test --release -- --ignored exhaustive`.
#[test]
#[ignore]
fn exhaustive_four_changes() {
    let (states, transitions) = explore(4, 12, false, |_| {});
    eprintln!("checked {transitions} transitions over {states} states (≤4 changes)");
}
