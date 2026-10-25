//! Model-based fuzzing of the restack engine against real git.
//!
//! Each change owns one file, `<name>.txt`, holding its current version, and
//! the model records every commit made for it. Random sequences of new,
//! amend, trunk moves, merges, reorders (link/unlink/move/land) and scoped
//! restacks run against a real repository. After each full restack every
//! branch must hold exactly the files of itself, its physical ancestors and
//! what's merged, at the model's versions; its own commits must sit directly
//! on its parent; and restacking again must move nothing. Every seed also runs
//! with GH_DAGS_NO_REPLAY, so the `git rebase` path is held to the same model.
//!
//! On failure the seed and the action log are printed. Reproduce with
//! `FUZZ_SEED=<n> cargo test --test fuzz -- --nocapture`; run more with
//! `FUZZ_SEEDS=<count> cargo test --release --test fuzz -- --ignored`.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::Sandbox;
use gh_dags::layout;
use gh_dags::model::{Base, State};
use gh_dags::store::Repo;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        (!items.is_empty()).then(|| &items[self.below(items.len())])
    }
    /// Up to `k` distinct items.
    fn sample<T: Clone>(&mut self, items: &[T], k: usize) -> Vec<T> {
        let mut items = items.to_vec();
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
        items.truncate(k);
        items
    }
}

#[derive(Default)]
struct Model {
    /// Current content version of each change's file (merged ones too).
    version: BTreeMap<String, u32>,
    /// Commit subjects of each tracked change, oldest first.
    own: BTreeMap<String, Vec<String>>,
    trunk: u32,
    merged: BTreeSet<String>,
}

/// How often the interesting cases came up, so a test can insist on them.
#[derive(Debug, Default, Clone, Copy)]
struct Coverage {
    parked_checked: usize,
    moved_down: usize,
    hoisted: usize,
    merges: usize,
    scoped: usize,
    full: usize,
}

impl std::ops::AddAssign for Coverage {
    fn add_assign(&mut self, o: Coverage) {
        self.parked_checked += o.parked_checked;
        self.moved_down += o.moved_down;
        self.hoisted += o.hoisted;
        self.merges += o.merges;
        self.scoped += o.scoped;
        self.full += o.full;
    }
}

struct World {
    sb: Sandbox,
    m: Model,
    rng: Rng,
    log: Vec<String>,
    next: usize,
    cov: Coverage,
    /// PRs live on the fake GitHub, so merges go through `land`, not git.
    github: bool,
}

impl World {
    fn new(seed: u64, rebase: bool) -> World {
        let mut sb = Sandbox::new();
        if rebase {
            sb.env.push(("GH_DAGS_NO_REPLAY".into(), "1".into()));
        }
        World {
            sb,
            m: Model::default(),
            rng: Rng(seed ^ 0x9e37_79b9_7f4a_7c15),
            log: Vec::new(),
            next: 0,
            cov: Coverage::default(),
            github: false,
        }
    }

    fn state(&self) -> State {
        Repo::open(&self.sb.work).unwrap().load().unwrap()
    }

    fn dags(&mut self, args: &[&str]) -> (bool, String) {
        let (ok, out) = self.sb.dags(args);
        self.log.push(format!(
            "gh dags {} -> {}",
            args.join(" "),
            if ok { "ok" } else { "rejected" }
        ));
        (ok, out)
    }

    fn commit(&mut self, change: &str) {
        let v = self.m.version.entry(change.to_string()).or_insert(0);
        *v += 1;
        let subject = format!("{change} v{v}");
        self.sb.git(&["checkout", "-q", change]);
        self.sb
            .commit_file(&format!("{change}.txt"), &format!("{subject}\n"), &subject);
        self.m
            .own
            .entry(change.to_string())
            .or_default()
            .push(subject.clone());
        self.log.push(format!("commit {subject}"));
    }

    fn step(&mut self) {
        let st = self.state();
        let names: Vec<String> = st.changes.keys().cloned().collect();
        let roll = self.rng.below(100);
        match roll {
            0..=19 if names.len() < 8 => self.new_change(&st, &names),
            0..=33 if !names.is_empty() => {
                let c = self.rng.pick(&names).unwrap().clone();
                self.commit(&c);
            }
            0..=38 => {
                self.m.trunk += 1;
                let subject = format!("main v{}", self.m.trunk);
                self.sb.git(&["checkout", "-q", "main"]);
                self.sb
                    .commit_file("main.txt", &format!("{subject}\n"), &subject);
                self.sb.git(&["push", "-q", "origin", "main"]);
                self.log.push(format!("trunk {subject}"));
            }
            0..=44 if names.len() >= 2 => {
                let (c, d) = (
                    self.rng.pick(&names).unwrap().clone(),
                    self.rng.pick(&names).unwrap().clone(),
                );
                self.dags(&["link", &c, &d]);
            }
            0..=50 if !names.is_empty() => {
                let c = self.rng.pick(&names).unwrap().clone();
                let deps: Vec<String> = st.changes[&c].deps.iter().cloned().collect();
                if let Some(d) = self.rng.pick(&deps).cloned() {
                    self.dags(&["unlink", &c, &d]);
                }
            }
            // Down onto an ancestor (or trunk): the case that has to drop
            // commits. A leaf, minus the deps the target doesn't cover, always
            // fits there.
            0..=60 if !names.is_empty() => self.detach(&st, &names),
            0..=64 if !names.is_empty() => {
                let c = self.rng.pick(&names).unwrap().clone();
                let mut targets = names.clone();
                targets.push("main".into());
                let onto = self.rng.pick(&targets).unwrap().clone();
                self.dags(&["move", &c, "--onto", &onto]);
            }
            0..=73 if !names.is_empty() => {
                // Prefer a change that has something to hoist past.
                let falsely: Vec<String> = names
                    .iter()
                    .filter(|n| !st.false_deps(n).is_empty())
                    .cloned()
                    .collect();
                let c = self
                    .rng
                    .pick(if falsely.is_empty() { &names } else { &falsely })
                    .unwrap()
                    .clone();
                let (ok, out) = self.dags(&["land", &c, "--local"]);
                if ok && out.contains("layout:") {
                    self.cov.hoisted += 1;
                }
            }
            0..=77 if !self.github => self.merge(&st),
            0..=88 if !names.is_empty() => self.scoped_restack(&st, &names),
            _ => self.full_restack(),
        }
    }

    /// A new change on 0-3 deps; half the time the deps are stack tops,
    /// which are often independent, so fan-in (and parking) actually happens.
    fn new_change(&mut self, st: &State, names: &[String]) {
        let name = format!("c{}", self.next);
        self.next += 1;
        let k = match self.rng.below(10) {
            0..=2 => 0,
            3..=5 => 1,
            6..=8 => 2,
            _ => 3,
        };
        // Fan-in from the tops of different stacks, which are independent;
        // otherwise any changes at all.
        let tops: Vec<String> = st
            .stacks
            .iter()
            .map(|s| s.changes.last().unwrap().clone())
            .collect();
        // With one stack there's nothing independent to fan in from yet:
        // start another stack instead.
        let k = if k >= 2 && tops.len() < 2 { 0 } else { k };
        // One dep on a change that isn't a stack top starts a spur.
        let inner: Vec<String> = names
            .iter()
            .filter(|n| !tops.contains(*n))
            .cloned()
            .collect();
        let pool = if k >= 2 {
            tops
        } else if k == 1 && !inner.is_empty() && self.rng.below(2) == 0 {
            inner
        } else {
            names.to_vec()
        };
        let mut deps = self.rng.sample(&pool, k);
        deps.sort();
        let on = if deps.is_empty() {
            "main".to_string()
        } else {
            deps.join(",")
        };
        let mut args = vec!["new", name.as_str(), "--on", on.as_str()];
        if deps.len() >= 2 && self.rng.below(2) == 0 {
            args.push("--park");
        }
        if self.dags(&args).0 {
            self.commit(&name);
        }
    }

    fn detach(&mut self, st: &State, names: &[String]) {
        let leaves: Vec<String> = names
            .iter()
            .filter(|n| st.children(n).is_empty())
            .cloned()
            .collect();
        let Some(c) = self.rng.pick(&leaves).cloned() else {
            return;
        };
        let parent = st.parent(&c);
        let mut targets: Vec<Option<String>> = st
            .phys_ancestors(&c)
            .into_iter()
            .filter(|a| parent != Some(Base::Change(a.clone())))
            .map(Some)
            .collect();
        if parent != Some(Base::Trunk) {
            targets.push(None);
        }
        let Some(target) = self.rng.pick(&targets).cloned() else {
            return;
        };
        let keep = match &target {
            Some(t) => {
                let mut k = st.phys_ancestors(t);
                k.insert(t.clone());
                k
            }
            None => BTreeSet::new(),
        };
        for d in st.changes[&c].deps.iter().filter(|d| !keep.contains(*d)) {
            self.dags(&["unlink", &c, d]);
        }
        let onto = target.unwrap_or_else(|| "main".into());
        let (ok, out) = self.dags(&["move", &c, "--onto", &onto]);
        if !ok {
            self.fail(&format!("detaching {c} onto {onto} was rejected:\n{out}"));
        }
        self.cov.moved_down += 1;
    }

    /// What GitHub does when the bottom of a stack lands: squash it into
    /// trunk. Then gh-dags absorbs it (as `sync` would).
    fn merge(&mut self, st: &State) {
        let bottoms: Vec<String> = st
            .stacks
            .iter()
            .filter(|s| s.base == Base::Trunk)
            .map(|s| s.changes[0].clone())
            .collect();
        let Some(m) = self.rng.pick(&bottoms).cloned() else {
            return;
        };
        // Like `land`, merge only what's restacked.
        self.full_restack();
        self.sb.git(&["checkout", "-q", "main"]);
        let tree = self.sb.git(&["merge-tree", "--write-tree", "main", &m]);
        let tree = tree.lines().next().unwrap();
        let commit = self.sb.git(&[
            "commit-tree",
            tree,
            "-p",
            "main",
            "-m",
            &format!("{m} (squash)"),
        ]);
        self.sb.git(&["reset", "-q", "--hard", &commit]);
        self.sb.git(&["push", "-q", "origin", "main"]);
        let repo = Repo::open(&self.sb.work).unwrap();
        let mut st = repo.load().unwrap();
        layout::absorb_merged(&mut st, &m)
            .unwrap_or_else(|e| self.fail(&format!("absorbing {m}: {e:#}")));
        repo.save(&st).unwrap();
        self.cov.merges += 1;
        self.m.merged.insert(m.clone());
        self.m.own.remove(&m);
        self.log.push(format!("merged {m}"));
    }

    fn tips(&self, st: &State) -> BTreeMap<String, String> {
        st.changes
            .keys()
            .map(|n| (n.clone(), self.sb.tip(n)))
            .collect()
    }

    fn scoped_restack(&mut self, st: &State, names: &[String]) {
        let c = self.rng.pick(names).unwrap().clone();
        let up = self.rng.below(2) == 0;
        let scope: BTreeSet<String> = if up {
            st.subtree(&c)
        } else {
            let mut s = st.phys_ancestors(&c);
            s.insert(c.clone());
            s
        };
        let before = self.tips(st);
        let flag = if up { "--upstack" } else { "--downstack" };
        self.cov.scoped += 1;
        let (ok, out) = self.dags(&["restack", &c, flag]);
        if !ok {
            self.fail(&format!("scoped restack failed:\n{out}"));
        }
        let after = self.tips(st);
        for (n, tip) in &before {
            if !scope.contains(n) && after[n] != *tip {
                self.fail(&format!(
                    "{n} moved though it's outside the {flag} scope of {c}"
                ));
            }
        }
        // In the scope, each change sits on its parent's current tip.
        let now = self.state();
        for n in now
            .topo_order()
            .unwrap()
            .iter()
            .filter(|n| scope.contains(*n))
        {
            self.check_placement(&now, n);
        }
    }

    fn full_restack(&mut self) {
        self.cov.full += 1;
        let (ok, out) = self.dags(&["restack"]);
        if !ok {
            self.fail(&format!("restack failed:\n{out}"));
        }
        let st = self.state();
        for n in st.changes.keys() {
            self.check_placement(&st, n);
            self.check_contents(&st, n);
        }
        let (_, out) = self.dags(&["restack"]);
        if !out.contains("already in place") {
            self.fail(&format!("a second restack moved something:\n{out}"));
        }
    }

    /// The top commits of `n` are exactly its own, and below them is its
    /// parent's tip (for a parked change: a merge of the sum's tips).
    fn check_placement(&mut self, st: &State, n: &str) {
        let own = &self.m.own[n];
        let k = own.len();
        let subjects = self
            .sb
            .git(&["log", "--reverse", "--format=%s", "-n", &k.to_string(), n]);
        let subjects: Vec<&str> = subjects.lines().collect();
        if subjects != own.iter().map(String::as_str).collect::<Vec<_>>() {
            self.fail(&format!(
                "{n}'s top commits are {subjects:?}, expected {own:?}"
            ));
        }
        let below = self.sb.tip(&format!("{n}~{k}"));
        // The recorded fork point has to be exactly where its commits start,
        // or a later restack would replay the wrong range.
        if st.changes[n].fork.as_deref() != Some(below.as_str()) {
            self.fail(&format!(
                "{n}'s recorded fork point isn't where its commits start"
            ));
        }
        match st.parent(n).unwrap() {
            Base::Trunk => {
                if below != self.sb.tip("origin/main") {
                    self.fail(&format!("{n} doesn't sit on origin/main"));
                }
            }
            Base::Change(p) => {
                if below != self.sb.tip(&p) {
                    self.fail(&format!("{n} doesn't sit on {p}'s tip"));
                }
            }
            Base::Sum(ms) => {
                self.cov.parked_checked += 1;
                let line = self.sb.git(&["rev-list", "--parents", "-n1", &below]);
                let parents: Vec<&str> = line.split(' ').skip(1).collect();
                let want: Vec<String> = ms.iter().map(|m| self.sb.tip(m)).collect();
                if parents != want.iter().map(String::as_str).collect::<Vec<_>>() {
                    self.fail(&format!("{n} doesn't sit on the sum of {ms:?}"));
                }
            }
        }
    }

    /// `n` holds exactly the files of itself, its physical ancestors, and
    /// what's merged, each at its current version.
    fn check_contents(&self, st: &State, n: &str) {
        let mut want = BTreeMap::new();
        let owners = st
            .phys_ancestors(n)
            .into_iter()
            .chain([n.to_string()])
            .chain(self.m.merged.iter().cloned());
        for o in owners {
            want.insert(format!("{o}.txt"), format!("{o} v{}", self.m.version[&o]));
        }
        if self.m.trunk > 0 {
            want.insert("main.txt".into(), format!("main v{}", self.m.trunk));
        }
        let files = self.sb.git(&["ls-tree", "-r", "--name-only", n]);
        let mut got = BTreeMap::new();
        for f in files.lines().filter(|f| f.ends_with(".txt")) {
            got.insert(f.to_string(), self.sb.show(n, f));
        }
        if got != want {
            self.fail(&format!("{n} holds {got:?}\nexpected {want:?}"));
        }
    }

    fn fail(&self, msg: &str) -> ! {
        let tail: Vec<&String> = self
            .log
            .iter()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let view = self.sb.dags(&["view"]).1;
        panic!(
            "{msg}\n\nlayout:\n{view}\nlast actions:\n  {}",
            tail.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
}

fn run(seed: u64, steps: usize, rebase: bool) -> Coverage {
    let mut w = World::new(seed, rebase);
    w.log.push(format!("seed {seed}, rebase path: {rebase}"));
    for _ in 0..steps {
        w.step();
    }
    w.full_restack();
    if std::env::var_os("FUZZ_LOG").is_some() {
        eprintln!("{}", w.log.join("\n"));
    }
    w.cov
}

/// Run the seeds, and insist the interesting cases actually came up.
fn fuzz(seeds: Vec<u64>, steps: usize, floor: usize, rebase: impl Fn(u64) -> bool) {
    let mut cov = Coverage::default();
    let results: Vec<Coverage> = std::thread::scope(|scope| {
        let handles: Vec<_> = seeds
            .iter()
            .map(|&seed| {
                let rebase = rebase(seed);
                scope.spawn(move || run(seed, steps, rebase))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    for c in results {
        cov += c;
    }
    eprintln!("coverage: {cov:?}");
    if std::env::var("FUZZ_SEED").is_err() {
        assert!(
            cov.parked_checked >= floor,
            "too few parked changes checked: {cov:?}"
        );
        assert!(
            cov.moved_down >= floor,
            "too few successful move-downs: {cov:?}"
        );
        assert!(
            cov.hoisted >= floor,
            "too few hoists that reordered: {cov:?}"
        );
        assert!(cov.merges >= floor && cov.scoped >= floor, "{cov:?}");
    }
}

fn seeds(default: std::ops::Range<u64>) -> Vec<u64> {
    if let Ok(s) = std::env::var("FUZZ_SEED") {
        return vec![s.parse().unwrap()];
    }
    match std::env::var("FUZZ_SEEDS") {
        Ok(n) => (0..n.parse().unwrap()).collect(),
        Err(_) => default.collect(),
    }
}

#[test]
fn restack_matches_the_model_via_replay() {
    fuzz(seeds(0..3), 60, 2, |_| false);
}

#[test]
fn restack_matches_the_model_via_rebase() {
    fuzz(seeds(100..103), 60, 2, |_| true);
}

#[test]
#[ignore]
fn restack_fuzz_long() {
    fuzz(seeds(1000..1040), 80, 40, |seed| seed % 2 == 0);
}

/// Make `descendant` edit `owner`'s file, then amend `owner` so replaying
/// that edit conflicts. Restack must stop; then either `--abort` restores
/// everything exactly, or resolving and `--continue` finishes the job.
/// Returns whether it aborted.
fn conflict_case(seed: u64, rebase: bool) -> Option<bool> {
    let mut w = World::new(seed, rebase);
    for _ in 0..3 + w.rng.below(4) {
        let st = w.state();
        let names: Vec<String> = st.changes.keys().cloned().collect();
        w.new_change(&st, &names);
    }
    w.full_restack();
    let st = w.state();
    let pairs: Vec<(String, String)> = st
        .changes
        .keys()
        .flat_map(|x| {
            st.subtree(x)
                .into_iter()
                .filter(move |y| y != x)
                .map(move |y| (x.clone(), y))
        })
        .collect();
    let (x, y) = w.rng.pick(&pairs)?.clone();

    w.sb.git(&["checkout", "-q", &y]);
    let edit = format!("{y} edits {x}");
    w.sb.commit_file(&format!("{x}.txt"), &format!("{edit}\n"), &edit);
    w.m.own.get_mut(&y).unwrap().push(edit);
    w.commit(&x);
    let names: Vec<String> = st.changes.keys().cloned().collect();
    let start = w.rng.pick(&names).unwrap().clone();
    w.sb.git(&["checkout", "-q", &start]);
    w.log.push(format!(
        "conflict: {y} edits {x}.txt, then {x} amends it; on {start}"
    ));

    let state_file = w.sb.work.join(".git/gh-dags/state.json");
    let journal = w.sb.work.join(".git/gh-dags/restack.json");
    let before_state = std::fs::read_to_string(&state_file).unwrap();
    let before_tips = w.tips(&st);
    let (ok, out) = w.dags(&["restack"]);
    if ok || !out.contains("conflict") {
        w.fail(&format!("expected a conflict:\n{out}"));
    }

    let abort = w.rng.below(2) == 0;
    if abort {
        let (ok, out) = w.dags(&["restack", "--abort"]);
        if !ok {
            w.fail(&format!("abort failed:\n{out}"));
        }
        if w.tips(&st) != before_tips {
            w.fail("abort didn't restore every branch");
        }
        if std::fs::read_to_string(&state_file).unwrap() != before_state {
            w.fail("abort didn't restore the state file exactly");
        }
        if w.sb.branch() != start || journal.exists() {
            w.fail("abort left HEAD or the journal behind");
        }
    } else {
        for _ in 0..10 {
            w.sb.commit_file_staged_only(&format!("{x}.txt"), "resolved\n");
            let (ok, out) = w.dags(&["restack", "--continue"]);
            if ok {
                break;
            }
            if !out.contains("conflict") {
                w.fail(&format!("--continue failed:\n{out}"));
            }
        }
        let st = w.state();
        for n in st.changes.keys() {
            w.check_placement(&st, n);
        }
        if w.sb.show(&y, &format!("{x}.txt")) != "resolved" {
            w.fail("the resolution didn't stick");
        }
        if !w.dags(&["restack"]).1.contains("already in place") || w.sb.branch() != start {
            w.fail("after --continue, things weren't in place, or HEAD moved");
        }
    }
    Some(abort)
}

fn conflicts(seeds: Vec<u64>, rebase: bool) {
    let (mut aborted, mut continued) = (0, 0);
    for seed in seeds {
        match conflict_case(seed, rebase) {
            Some(true) => aborted += 1,
            Some(false) => continued += 1,
            None => {}
        }
    }
    eprintln!("conflicts: {aborted} aborted, {continued} continued");
    if std::env::var("FUZZ_SEED").is_err() {
        assert!(
            aborted >= 1 && continued >= 1,
            "{aborted} aborted, {continued} continued"
        );
    }
}

#[test]
fn conflicts_abort_or_continue_cleanly() {
    conflicts(seeds(200..206), false);
}

#[test]
fn conflicts_abort_or_continue_cleanly_via_rebase() {
    conflicts(seeds(300..306), true);
}

#[derive(Debug, Default)]
struct GithubCoverage {
    submits: usize,
    rebuilt: usize,
    spur_stacks: usize,
    merged: usize,
    /// Lands that merged several PRs at once (a stack prefix).
    multi: usize,
}

impl World {
    fn base_branch(st: &State, n: &str) -> String {
        match st.parent(n) {
            Some(Base::Change(p)) => p,
            _ => "main".into(),
        }
    }

    /// GitHub agrees with the layout, and no PR was ever implicitly merged.
    /// `strict` (after `submit`): every submittable change has a PR. After
    /// `sync`, a change that just unparked may not have one yet.
    fn check_github(&self, cov: &mut GithubCoverage, strict: bool) {
        let gh = self.sb.github();
        let st = self.state();
        if let Some(prs) = gh.get("implicitly_merged") {
            self.fail(&format!(
                "GitHub implicitly merged {prs}: a base came to contain its head"
            ));
        }
        let mut layout_stacks: Vec<(String, Vec<u64>)> = Vec::new();
        for s in &st.stacks {
            if st.is_parked(&s.changes[0]) {
                continue;
            }
            // Like submit: a stack on GitHub runs up to the first change without a PR.
            let prs: Vec<u64> = s.changes.iter().map_while(|c| st.changes[c].pr).collect();
            layout_stacks.push((Self::base_branch(&st, &s.changes[0]), prs));
        }
        for (name, c) in &st.changes {
            if st.is_parked(name) {
                continue;
            }
            let remote = self
                .sb
                .git(&["ls-remote", "origin", &format!("refs/heads/{name}")]);
            if remote.split('\t').next() != Some(self.sb.tip(name).as_str()) {
                self.fail(&format!("origin/{name} isn't the local {name}"));
            }
            let Some(n) = c.pr else {
                if strict {
                    self.fail(&format!("{name} has no PR"));
                }
                continue;
            };
            let pr = &gh["prs"][n.to_string()];
            if pr["state"] != "open" || pr["head"] != name.as_str() {
                self.fail(&format!(
                    "#{n} for {name} isn't open with head {name}: {pr}"
                ));
            }
            let base = Self::base_branch(&st, name);
            if pr["base"] != base.as_str() {
                self.fail(&format!("#{n} ({name}) targets {}, not {base}", pr["base"]));
            }
            let body = pr["body"].as_str().unwrap_or_default();
            let meta = body
                .split("<!-- gh-dags:meta ")
                .nth(1)
                .and_then(|m| m.split(" -->").next())
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok());
            let want: Vec<&str> = c.deps.iter().map(String::as_str).collect();
            if meta.as_ref().map(|m| m["deps"].clone()) != Some(serde_json::json!(want)) {
                self.fail(&format!(
                    "#{n} ({name})'s annotation has deps {meta:?}, expected {want:?}"
                ));
            }
        }
        let open: Vec<(String, Vec<u64>)> = gh["stacks"]
            .as_object()
            .unwrap()
            .values()
            .filter(|s| s["open"] == true)
            .map(|s| {
                let prs = s["prs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap())
                    .filter(|n| gh["prs"][n.to_string()]["state"] == "open")
                    .collect();
                (s["base"].as_str().unwrap().to_string(), prs)
            })
            .collect();
        for (base, prs) in &layout_stacks {
            if prs.len() >= 2 && !open.contains(&(base.clone(), prs.clone())) {
                self.fail(&format!(
                    "no open GitHub stack {prs:?} on {base}; open: {open:?}"
                ));
            }
            if prs.len() >= 2 && base != "main" {
                cov.spur_stacks += 1;
            }
        }
        for (base, prs) in &open {
            if !prs.is_empty() && !layout_stacks.contains(&(base.clone(), prs.clone())) {
                self.fail(&format!("GitHub has a stale stack {prs:?} on {base}"));
            }
        }
    }

    /// Add a change on a spur's top, or start a spur off an inner change.
    fn grow_spur(&mut self) {
        let st = self.state();
        let spur_tops: Vec<String> = st
            .stacks
            .iter()
            .filter(|s| matches!(s.base, Base::Change(_)))
            .map(|s| s.changes.last().unwrap().clone())
            .collect();
        let inner: Vec<String> = st
            .changes
            .keys()
            .filter(|n| !st.is_top(n))
            .cloned()
            .collect();
        let Some(on) = self
            .rng
            .pick(if spur_tops.is_empty() {
                &inner
            } else {
                &spur_tops
            })
            .cloned()
        else {
            return;
        };
        // A new spur gets two changes, so it's a stack on GitHub right away.
        let mut on = on;
        for _ in 0..if spur_tops.is_empty() { 2 } else { 1 } {
            let name = format!("c{}", self.next);
            self.next += 1;
            if !self.dags(&["new", &name, "--on", &on]).0 {
                return;
            }
            self.commit(&name);
            on = name;
        }
    }

    fn land_for_real(&mut self, cov: &mut GithubCoverage) {
        // Land something that needs other changes, so a prefix of several
        // PRs merges at once; make one if there's none.
        let deep = |st: &State| -> Vec<String> {
            st.changes
                .iter()
                .filter(|(n, c)| {
                    c.pr.is_some() && !st.is_parked(n) && !st.logical_ancestors(n).is_empty()
                })
                .map(|(n, _)| n.clone())
                .collect()
        };
        let mut st = self.state();
        if deep(&st).is_empty() {
            let on: Vec<String> = st
                .changes
                .iter()
                .filter(|(n, c)| c.pr.is_some() && !st.is_parked(n))
                .map(|(n, _)| n.clone())
                .collect();
            let Some(on) = self.rng.pick(&on).cloned() else {
                return;
            };
            let name = format!("c{}", self.next);
            self.next += 1;
            if !self.dags(&["new", &name, "--on", &on]).0 {
                return;
            }
            self.commit(&name);
            let (ok, out) = self.dags(&["submit"]);
            if !ok {
                self.fail(&format!("submit failed:\n{out}"));
            }
            st = self.state();
        }
        let Some(c) = self.rng.pick(&deep(&st)).cloned() else {
            return;
        };
        let before: BTreeSet<String> = st.changes.keys().cloned().collect();
        for _ in 0..3 {
            let (ok, out) = self.dags(&["land", &c, "--method", "squash"]);
            if !ok {
                self.fail(&format!("land {c} failed:\n{out}"));
            }
            self.check_github(cov, false);
            if out.contains("merged #") {
                break;
            }
        }
        let after: BTreeSet<String> = self.state().changes.keys().cloned().collect();
        let gone: Vec<&String> = before.difference(&after).collect();
        if gone.len() >= 2 {
            cov.multi += 1;
        }
        for m in gone {
            self.m.merged.insert(m.clone());
            self.m.own.remove(m);
            cov.merged += 1;
        }
        self.full_restack();
    }
}

fn github_case(seed: u64, rounds: usize) -> GithubCoverage {
    let mut cov = GithubCoverage::default();
    let mut w = World::new(seed, false);
    w.github = true;
    w.log.push(format!("seed {seed}, against the fake GitHub"));
    for _ in 0..rounds {
        for _ in 0..2 + w.rng.below(4) {
            w.step();
        }
        if w.rng.below(2) == 0 {
            w.grow_spur();
        }
        // Fan in across two stacks already on GitHub: linearizing puts one
        // on the other, so the next submit has to rebuild both.
        if w.rng.below(2) == 0 {
            let st = w.state();
            let tops: Vec<String> = st
                .stacks
                .iter()
                .filter(|s| !st.is_parked(&s.changes[0]))
                .map(|s| s.changes.last().unwrap().clone())
                .filter(|t| st.changes[t].pr.is_some())
                .collect();
            let pair = w.rng.sample(&tops, 2);
            if pair.len() == 2 {
                let name = format!("c{}", w.next);
                w.next += 1;
                if w.dags(&["new", &name, "--on", &pair.join(",")]).0 {
                    w.commit(&name);
                }
            }
        }
        // Reorder before submitting, so stacks on GitHub have to be rebuilt.
        if w.rng.below(2) == 0 {
            let st = w.state();
            // Something already on GitHub with something to hoist past.
            let falsely: Vec<String> = st
                .changes
                .iter()
                .filter(|(n, c)| c.pr.is_some() && !st.false_deps(n).is_empty())
                .map(|(n, _)| n.clone())
                .collect();
            if let Some(c) = w.rng.pick(&falsely).cloned() {
                w.dags(&["land", &c, "--local"]);
            }
        }
        let (ok, out) = w.dags(&["submit"]);
        if !ok {
            w.fail(&format!("submit failed:\n{out}"));
        }
        cov.submits += 1;
        if out.contains("dissolved") {
            cov.rebuilt += 1;
        }
        w.check_github(&mut cov, true);
        if w.rng.below(2) == 0 {
            w.land_for_real(&mut cov);
        }
    }
    if std::env::var_os("FUZZ_LOG").is_some() {
        eprintln!("{}", w.log.join("\n"));
    }
    cov
}

fn github(seeds: Vec<u64>, rounds: usize) {
    let results: Vec<GithubCoverage> = std::thread::scope(|scope| {
        let handles: Vec<_> = seeds
            .iter()
            .map(|&s| scope.spawn(move || github_case(s, rounds)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    let total = results.iter().fold(GithubCoverage::default(), |mut t, c| {
        t.submits += c.submits;
        t.rebuilt += c.rebuilt;
        t.spur_stacks += c.spur_stacks;
        t.merged += c.merged;
        t.multi += c.multi;
        t
    });
    eprintln!("github coverage: {total:?}");
    if std::env::var("FUZZ_SEED").is_err() {
        assert!(
            total.rebuilt >= 1 && total.multi >= 1 && total.spur_stacks >= 1,
            "{total:?}"
        );
    }
}

#[test]
fn submit_land_and_sync_keep_github_consistent() {
    github(seeds(400..403), 6);
}

#[test]
#[ignore]
fn github_fuzz_long() {
    github(seeds(4000..4012), 12);
}

#[test]
#[ignore]
fn conflicts_long() {
    conflicts(seeds(2000..2040), false);
    conflicts(seeds(3000..3040), true);
}
