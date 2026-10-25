//! The persistent state: the logical DAG (each change's deps) plus the
//! physical layout (stacks), and the graph queries both are built on.
//!
//! The one invariant (see DESIGN.md §2): every dependency of a change is one
//! of its physical ancestors. [`State::validate`] checks it, along with the
//! structural rules that make the layout a forest.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;

/// What the bottom of a stack sits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Base {
    /// The trunk branch: a regular stack.
    Trunk,
    /// Another change: a spur.
    Change(String),
    /// A synthetic merge of several changes: a parked stack (local only).
    Sum(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Logical parents. Empty means the change depends only on trunk.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub deps: BTreeSet<String>,
    /// Commit of the physical parent this change was last restacked onto.
    /// Restacking replays exactly `fork..branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// The commit we last pushed for this change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<String>,
    /// Creation order; breaks ties so newer work moves, not older.
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    pub base: Base,
    /// Bottom to top.
    pub changes: Vec<String>,
    /// The GitHub stack number, once submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub trunk: String,
    pub remote: String,
    pub next_seq: u64,
    pub changes: BTreeMap<String, Change>,
    pub stacks: Vec<Stack>,
}

impl State {
    pub fn new(trunk: &str, remote: &str) -> Self {
        State {
            version: STATE_VERSION,
            trunk: trunk.to_string(),
            remote: remote.to_string(),
            next_seq: 0,
            changes: BTreeMap::new(),
            stacks: Vec::new(),
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.changes.contains_key(name)
    }

    pub fn change(&self, name: &str) -> Result<&Change> {
        match self.changes.get(name) {
            Some(c) => Ok(c),
            None => bail!("{name:?} is not a tracked change"),
        }
    }

    pub fn change_mut(&mut self, name: &str) -> Result<&mut Change> {
        match self.changes.get_mut(name) {
            Some(c) => Ok(c),
            None => bail!("{name:?} is not a tracked change"),
        }
    }

    /// (stack index, position within the stack).
    pub fn locate(&self, name: &str) -> Option<(usize, usize)> {
        self.stacks.iter().enumerate().find_map(|(si, s)| {
            s.changes
                .iter()
                .position(|c| c == name)
                .map(|pos| (si, pos))
        })
    }

    pub fn stack_of(&self, name: &str) -> Option<&Stack> {
        self.locate(name).map(|(si, _)| &self.stacks[si])
    }

    /// The physical parent: the predecessor in the stack, or the stack's base.
    pub fn parent(&self, name: &str) -> Option<Base> {
        let (si, pos) = self.locate(name)?;
        let stack = &self.stacks[si];
        Some(if pos == 0 {
            stack.base.clone()
        } else {
            Base::Change(stack.changes[pos - 1].clone())
        })
    }

    pub fn is_top(&self, name: &str) -> bool {
        self.stack_of(name)
            .is_some_and(|s| s.changes.last().map(String::as_str) == Some(name))
    }

    /// Everything reachable by following physical parents (excluding trunk).
    pub fn phys_ancestors(&self, name: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut todo = vec![name.to_string()];
        while let Some(n) = todo.pop() {
            let parents: Vec<String> = match self.parent(&n) {
                Some(Base::Change(p)) => vec![p],
                Some(Base::Sum(ms)) => ms,
                _ => vec![],
            };
            for p in parents {
                if out.insert(p.clone()) {
                    todo.push(p);
                }
            }
        }
        out
    }

    /// Everything reachable by following deps.
    pub fn logical_ancestors(&self, name: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut todo = vec![name.to_string()];
        while let Some(n) = todo.pop() {
            if let Some(c) = self.changes.get(&n) {
                for d in &c.deps {
                    if out.insert(d.clone()) {
                        todo.push(d.clone());
                    }
                }
            }
        }
        out
    }

    /// Direct physical children: the stack successor first, then the bottoms
    /// of stacks based on this change (spurs and sums), in stack order.
    pub fn children(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some((si, pos)) = self.locate(name)
            && let Some(next) = self.stacks[si].changes.get(pos + 1)
        {
            out.push(next.clone());
        }
        for s in &self.stacks {
            let based_here = match &s.base {
                Base::Change(b) => b == name,
                Base::Sum(ms) => ms.iter().any(|m| m == name),
                Base::Trunk => false,
            };
            if based_here && let Some(bottom) = s.changes.first() {
                out.push(bottom.clone());
            }
        }
        out
    }

    /// The change and all of its physical descendants.
    pub fn subtree(&self, name: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::from([name.to_string()]);
        let mut todo = vec![name.to_string()];
        while let Some(n) = todo.pop() {
            for c in self.children(&n) {
                if out.insert(c.clone()) {
                    todo.push(c);
                }
            }
        }
        out
    }

    /// Whether the change's physical ancestry passes through a sum.
    pub fn is_parked(&self, name: &str) -> bool {
        let mut cur = name.to_string();
        // Bounded walk; validate() rejects cycles, this just avoids hanging on one.
        for _ in 0..=self.changes.len() {
            match self.parent(&cur) {
                Some(Base::Sum(_)) => return true,
                Some(Base::Change(p)) => cur = p,
                _ => return false,
            }
        }
        false
    }

    /// Root-to-node path of an unparked change (excluding trunk), bottom first.
    pub fn path(&self, name: &str) -> Vec<String> {
        let mut out = vec![name.to_string()];
        let mut cur = name.to_string();
        while let Some(Base::Change(p)) = self.parent(&cur) {
            if out.contains(&p) {
                break;
            }
            out.push(p.clone());
            cur = p;
        }
        out.reverse();
        out
    }

    /// An edge `name → parent` is ordering-only when the change doesn't
    /// actually need its parent.
    pub fn ordering_only_parent(&self, name: &str) -> Option<String> {
        match self.parent(name)? {
            Base::Change(p) if !self.logical_ancestors(name).contains(&p) => Some(p),
            _ => None,
        }
    }

    /// Physical ancestors that are not logical ancestors.
    pub fn false_deps(&self, name: &str) -> BTreeSet<String> {
        let logical = self.logical_ancestors(name);
        self.phys_ancestors(name)
            .into_iter()
            .filter(|a| !logical.contains(a))
            .collect()
    }

    /// Physical topological order: every change after everything it sits on.
    /// Ties keep stack order, so the result is stable.
    pub fn topo_order(&self) -> Result<Vec<String>> {
        let mut indegree: BTreeMap<&str, usize> = BTreeMap::new();
        let mut rank: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for (si, s) in self.stacks.iter().enumerate() {
            for (pos, c) in s.changes.iter().enumerate() {
                let parents = match (pos, &s.base) {
                    (0, Base::Trunk) => 0,
                    (0, Base::Change(_)) => 1,
                    (0, Base::Sum(ms)) => ms.len(),
                    _ => 1,
                };
                indegree.insert(c, parents);
                rank.insert(c, (si, pos));
            }
        }
        let mut ready: Vec<&str> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(c, _)| *c)
            .collect();
        let mut out = Vec::with_capacity(indegree.len());
        while !ready.is_empty() {
            ready.sort_by_key(|c| std::cmp::Reverse(rank[c]));
            let c = ready.pop().unwrap();
            out.push(c.to_string());
            for child in self.children(c) {
                let child = rank.get_key_value(child.as_str()).map(|(k, _)| *k).unwrap();
                let d = indegree.get_mut(child).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(child);
                }
            }
        }
        if out.len() != indegree.len() {
            bail!("the physical layout has a cycle");
        }
        Ok(out)
    }

    /// Check every structural rule and the dependency invariant.
    pub fn validate(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for s in &self.stacks {
            if s.changes.is_empty() {
                bail!("internal: empty stack");
            }
            for c in &s.changes {
                if !self.changes.contains_key(c) {
                    bail!("internal: stack lists unknown change {c:?}");
                }
                if !seen.insert(c.as_str()) {
                    bail!("internal: {c:?} is in more than one stack");
                }
            }
            match &s.base {
                Base::Trunk => {}
                Base::Change(b) => {
                    if !self.changes.contains_key(b) {
                        bail!("internal: stack based on unknown change {b:?}");
                    }
                    if s.changes.contains(b) {
                        bail!("internal: stack based on its own change {b:?}");
                    }
                }
                Base::Sum(ms) => {
                    if ms.len() < 2 {
                        bail!("internal: a sum needs at least two changes");
                    }
                    for m in ms {
                        if !self.changes.contains_key(m) || s.changes.contains(m) {
                            bail!("internal: bad sum member {m:?}");
                        }
                    }
                }
            }
        }
        for name in self.changes.keys() {
            if !seen.contains(name.as_str()) {
                bail!("internal: {name:?} is not in any stack");
            }
        }
        // Physical acyclicity.
        self.topo_order()?;
        // Logical acyclicity and unknown deps.
        for (name, c) in &self.changes {
            for d in &c.deps {
                if !self.changes.contains_key(d) {
                    bail!("{name:?} depends on unknown change {d:?}");
                }
            }
            if self.logical_ancestors(name).contains(name) {
                bail!("dependency cycle through {name:?}");
            }
        }
        // The invariant.
        for (name, c) in &self.changes {
            let anc = self.phys_ancestors(name);
            if let Some(missing) = c.deps.iter().find(|d| !anc.contains(*d)) {
                bail!("{name:?} depends on {missing:?}, but isn't stacked on it");
            }
        }
        Ok(())
    }

    /// Remove deps that are implied by other deps.
    pub fn reduce_deps(&self, deps: &BTreeSet<String>) -> BTreeSet<String> {
        let implied: BTreeSet<String> = deps
            .iter()
            .flat_map(|d| self.logical_ancestors(d))
            .collect();
        deps.iter()
            .filter(|d| !implied.contains(*d))
            .cloned()
            .collect()
    }

    /// The maximal elements of `set` under the physical-ancestor order.
    pub fn phys_maximal(&self, set: &[String]) -> Vec<String> {
        let below: BTreeSet<String> = set.iter().flat_map(|m| self.phys_ancestors(m)).collect();
        let mut out: Vec<String> = set
            .iter()
            .filter(|m| !below.contains(*m))
            .cloned()
            .collect();
        out.dedup();
        out
    }

    /// Bottoms of the trunk-based stacks, in stack order.
    pub fn roots(&self) -> Vec<String> {
        self.stacks
            .iter()
            .filter(|s| s.base == Base::Trunk)
            .map(|s| s.changes[0].clone())
            .collect()
    }
}
