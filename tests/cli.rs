//! End-to-end tests: the real binary against throwaway repositories with a
//! bare `origin`, isolated from the user's git config.

mod common;

use std::fs;
use std::process::Command;

use common::{Sandbox, command};

#[test]
fn submit_land_and_sync_against_a_fake_github() {
    let sb = Sandbox::new();
    sb.ok(&["new", "a"]);
    sb.commit_file("a", "a\n", "Add a");
    sb.git(&["checkout", "-q", "main"]);
    sb.ok(&["new", "b"]);
    sb.commit_file("b", "b\n", "Add b");
    sb.ok(&["new", "c", "--on", "a,b"]);
    sb.commit_file("c", "c\n", "Add c");
    sb.git(&["checkout", "-q", "a"]);
    sb.ok(&["new", "d"]);
    sb.commit_file("d", "d\n", "Add d");
    // Layout: main ← a ← b ← c, plus a spur d on a.

    let out = sb.ok(&["submit"]);
    for line in [
        "opened #1 a → main",
        "opened #2 b → a",
        "opened #3 c → b",
        "opened #4 d → a",
        "stack #1: #1 ← #2 ← #3",
    ] {
        assert!(out.contains(line), "missing {line:?} in:\n{out}");
    }
    let gh = sb.github();
    let body = |n: u64| {
        gh["prs"][n.to_string()]["body"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(
        body(2).contains("Depends only on `main`. Stacked on #1 (`a`) for ordering only"),
        "{}",
        body(2)
    );
    assert!(
        body(3).contains("Depends on #1 (`a`), #2 (`b`)."),
        "{}",
        body(3)
    );
    assert_eq!(sb.stack(1), [1, 2, 3]);

    let calls = sb.github()["calls"].as_array().unwrap().len();
    let out = sb.ok(&["submit"]);
    assert!(
        !out.contains("pushed") && !out.contains("opened") && !out.contains("stack #"),
        "{out}"
    );
    let new_calls: Vec<String> = sb.github()["calls"].as_array().unwrap()[calls..]
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert!(
        new_calls.iter().all(|c| c.starts_with("GET")),
        "a no-op submit only reads: {new_calls:?}"
    );

    // Landing b first: b moves to the bottom and the GitHub stack is rebuilt.
    let out = sb.ok(&["land", "b"]);
    for line in [
        "b now on main (was a)",
        "dissolved GitHub stack #1",
        "retargeted #2 b → main",
        "stack #2: #2 ← #1 ← #3",
        "run `gh dags land b` again",
    ] {
        assert!(out.contains(line), "missing {line:?} in:\n{out}");
    }
    assert_eq!(sb.stack(2), [2, 1, 3]);
    assert_eq!(
        (sb.pr_base(1), sb.pr_base(3), sb.pr_base(4)),
        ("b".into(), "a".into(), "a".into())
    );
    assert!(
        sb.github().get("implicitly_merged").is_none(),
        "no PR's base ever contained its head: {}",
        sb.github()["implicitly_merged"]
    );

    // Now nothing needs to move, so it merges. GitHub rebases a and c onto the
    // new main; sync takes those, drops b, and restacks d onto the new a.
    let out = sb.ok(&["land", "b"]);
    for line in [
        "marked #2 ready for review",
        "merged #2",
        "merged: b",
        "took a from origin",
        "took c from origin",
        "restacked d",
        "in sync",
    ] {
        assert!(out.contains(line), "missing {line:?} in:\n{out}");
    }
    let view = sb.ok(&["view"]);
    assert!(!view.contains("○ b"), "{view}");
    assert_eq!(sb.show("main", "b"), "b", "b was squash-merged into main");
    assert_eq!(sb.own_commits("main", "a"), ["Add a"]);
    assert_eq!(sb.own_commits("a", "c"), ["Add c"]);
    assert_eq!(sb.own_commits("a", "d"), ["Add d"]);
    assert_eq!(sb.pr_base(1), "main");
    assert_eq!(
        sb.stack(2),
        [2, 1, 3],
        "the merged PR stays in the stack, the rest is kept"
    );
    assert_eq!(
        sb.tip("d"),
        sb.tip("origin/d"),
        "the restacked spur was pushed"
    );
}

#[test]
fn fan_in_linearizes_and_restacks_after_an_amend() {
    let sb = Sandbox::new();
    sb.ok(&["new", "auth"]);
    sb.commit_file("auth", "v1\n", "auth");
    sb.git(&["checkout", "-q", "main"]);
    sb.ok(&["new", "theme"]);
    sb.commit_file("theme", "dark\n", "theme");

    let out = sb.ok(&["new", "login", "--on", "auth,theme"]);
    assert!(
        out.contains("theme now on auth (was main) · ordering only"),
        "{out}"
    );
    assert!(out.contains("restacked theme"), "{out}");
    assert_eq!(sb.own_commits("auth", "theme"), ["theme"]);
    assert_eq!(sb.tip("login"), sb.tip("theme"));
    sb.commit_file("login", "form\n", "login");

    // Amend auth; restack carries theme and login along, each with only its own commits.
    sb.git(&["checkout", "-q", "auth"]);
    sb.commit_file("auth", "v2\n", "auth v2");
    let out = sb.ok(&["restack"]);
    assert!(out.contains("restacked theme, login"), "{out}");
    assert_eq!(sb.own_commits("main", "auth"), ["auth", "auth v2"]);
    assert_eq!(sb.own_commits("auth", "theme"), ["theme"]);
    assert_eq!(sb.own_commits("theme", "login"), ["login"]);
    assert_eq!(sb.show("login", "auth"), "v2");
    assert_eq!(
        sb.git(&["branch", "--show-current"]),
        "auth",
        "restack returns to the branch you were on"
    );

    let view = sb.ok(&["view"]);
    assert!(
        view.contains("⚠ ordered after auth (independent)"),
        "{view}"
    );
    assert!(view.contains("← auth, theme"), "{view}");
    assert!(sb.ok(&["restack"]).contains("already in place"));
}

#[test]
fn parked_change_sits_on_a_deterministic_sum() {
    let sb = Sandbox::new();
    sb.ok(&["new", "a"]);
    sb.commit_file("a", "a\n", "a");
    sb.git(&["checkout", "-q", "main"]);
    sb.ok(&["new", "b"]);
    sb.commit_file("b", "b\n", "b");

    sb.ok(&["new", "c", "--on", "a,b", "--park"]);
    assert_eq!(sb.parents("c"), 2, "c starts on the sum commit");
    assert!(sb.work.join("a").exists() && sb.work.join("b").exists());
    sb.commit_file("c", "c\n", "c");

    let view = sb.ok(&["view"]);
    assert!(view.contains("⊕ parked on a ⊕ b"), "{view}");
    assert!(
        sb.ok(&["restack"]).contains("already in place"),
        "the sum is reproducible"
    );

    sb.git(&["checkout", "-q", "a"]);
    sb.commit_file("a", "a2\n", "a2");
    let out = sb.ok(&["restack"]);
    assert!(out.contains("restacked c"), "{out}");
    assert_eq!(sb.show("c", "a"), "a2");
    assert_eq!(sb.own_commits("c~1", "c"), ["c"]);
    assert_eq!(sb.parents("c~1"), 2);
}

#[test]
fn conflicts_stop_and_continue_or_abort() {
    for finish in ["continue", "abort"] {
        let sb = Sandbox::new();
        sb.ok(&["new", "base"]);
        sb.commit_file("f", "one\n", "base");
        sb.ok(&["new", "top"]);
        sb.commit_file("f", "one\ntwo\n", "top");
        let top_before = sb.tip("top");

        sb.git(&["checkout", "-q", "base"]);
        sb.commit_file("f", "uno\n", "base edit");
        let (ok, out) = sb.dags(&["restack"]);
        assert!(!ok && out.contains("conflict in \"top\""), "{out}");
        let (ok, out) = sb.dags(&["new", "x"]);
        assert!(!ok && out.contains("restack is in progress"), "{out}");

        if finish == "continue" {
            fs::write(sb.work.join("f"), "uno\ntwo\n").unwrap();
            sb.git(&["add", "f"]);
            let out = sb.ok(&["restack", "--continue"]);
            assert!(out.contains("restacked top"), "{out}");
            assert_eq!(sb.show("top", "f"), "uno\ntwo");
            assert_eq!(sb.own_commits("base", "top"), ["top"]);
        } else {
            sb.ok(&["restack", "--abort"]);
            assert_eq!(sb.tip("top"), top_before);
        }
        assert_eq!(sb.git(&["branch", "--show-current"]), "base");
        assert!(sb.ok(&["view"]).contains("top"));
    }
}

#[test]
fn land_hoists_what_a_change_needs() {
    let sb = Sandbox::new();
    for name in ["a", "b"] {
        sb.git(&["checkout", "-q", "main"]);
        sb.ok(&["new", name]);
        sb.commit_file(name, "x\n", name);
    }
    sb.ok(&["new", "c", "--on", "a,b"]);
    sb.commit_file("c", "x\n", "c");
    assert_eq!(sb.own_commits("main", "b"), ["a", "b"]);

    let out = sb.ok(&["land", "b", "--local"]);
    assert!(out.contains("b now on main (was a)"), "{out}");
    assert_eq!(sb.own_commits("main", "b"), ["b"], "b no longer carries a");
    assert_eq!(sb.own_commits("b", "a"), ["a"]);
    assert_eq!(sb.own_commits("a", "c"), ["c"]);
    assert!(sb.ok(&["land", "b", "--local"]).contains("b is ready"));
}

#[test]
fn track_infers_deps_and_moves_are_validated() {
    let sb = Sandbox::new();
    sb.ok(&["new", "a"]);
    sb.commit_file("a", "x\n", "a");
    sb.git(&["checkout", "-q", "-b", "manual"]);
    sb.commit_file("m", "x\n", "manual");

    let out = sb.ok(&["track", "manual"]);
    assert!(out.contains("manual on a"), "{out}");
    let (ok, out) = sb.dags(&["move", "manual", "--onto", "main"]);
    assert!(!ok && out.contains("depends on \"a\""), "{out}");

    sb.ok(&["unlink", "manual", "a"]);
    let out = sb.ok(&["move", "manual", "--onto", "main"]);
    assert!(out.contains("manual now on main (was a)"), "{out}");
    assert_eq!(sb.own_commits("main", "manual"), ["manual"]);
}

impl Sandbox {
    fn git_ok(&self, args: &[&str]) -> bool {
        command("git", &self.work, self._tmp.path())
            .args(args)
            .status()
            .unwrap()
            .success()
    }

    /// Run a shell snippet in the work tree with the sandbox's environment.
    fn shell(&self, shell: &str, script: &str) -> String {
        let out = command(shell, &self.work, self._tmp.path())
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{shell} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

fn has(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok()
}

/// main ← auth ← tests, with theme ← login as a spur on auth.
fn forked(sb: &Sandbox) {
    sb.ok(&["new", "auth"]);
    sb.commit_file("auth", "x\n", "auth");
    sb.ok(&["new", "tests"]);
    sb.commit_file("tests", "x\n", "tests");
    sb.git(&["checkout", "-q", "auth"]);
    sb.ok(&["new", "theme"]);
    sb.commit_file("theme", "x\n", "theme");
    sb.ok(&["new", "login"]);
    sb.commit_file("login", "x\n", "login");
}

#[test]
fn navigation_follows_stacks_and_crosses_spurs() {
    let sb = Sandbox::new();
    forked(&sb);
    sb.ok(&["trunk"]);
    assert_eq!(sb.branch(), "main");

    sb.ok(&["up"]);
    assert_eq!(
        sb.branch(),
        "auth",
        "one stack on trunk, so up is unambiguous"
    );
    let out = sb.ok(&["up"]);
    assert_eq!(sb.branch(), "tests");
    assert!(
        out.contains("also branching off on the way: theme"),
        "{out}"
    );
    let (ok, out) = sb.dags(&["up"]);
    assert!(!ok && out.contains("already at the top"), "{out}");

    sb.ok(&["switch", "login"]);
    sb.ok(&["bottom"]);
    assert_eq!(sb.branch(), "theme", "bottom of the spur's stack");
    sb.ok(&["top"]);
    assert_eq!(sb.branch(), "login");
    sb.ok(&["down", "2"]);
    assert_eq!(
        sb.branch(),
        "auth",
        "down crosses from the spur into its anchor"
    );
    let (ok, out) = sb.dags(&["down"]);
    assert!(!ok && out.contains("already at the bottom"), "{out}");

    let (ok, out) = sb.dags(&["switch"]);
    assert!(
        !ok && out.contains("name the change"),
        "no picker without a terminal: {out}"
    );
    let (ok, out) = sb.dags(&["switch", "nope"]);
    assert!(!ok && out.contains("not a tracked change"), "{out}");
}

#[test]
fn restack_can_be_scoped() {
    let sb = Sandbox::new();
    sb.ok(&["new", "a"]);
    sb.commit_file("a", "1\n", "a");
    sb.ok(&["new", "b"]);
    sb.commit_file("b", "x\n", "b");
    sb.ok(&["new", "c"]);
    sb.commit_file("c", "x\n", "c");
    sb.git(&["checkout", "-q", "main"]);
    sb.ok(&["new", "d"]);
    sb.commit_file("d", "x\n", "d");
    // Trunk moves on, and a is amended.
    sb.git(&["checkout", "-q", "main"]);
    sb.commit_file("m", "x\n", "main moved");
    sb.git(&["push", "-q", "origin", "main"]);
    sb.git(&["checkout", "-q", "a"]);
    sb.commit_file("a", "2\n", "a2");
    let on_trunk = |b: &str| sb.git_ok(&["merge-base", "--is-ancestor", "origin/main", b]);

    let out = sb.ok(&["restack", "b", "--upstack"]);
    assert!(out.contains("restacked b, c"), "{out}");
    assert_eq!(sb.own_commits("a", "b"), ["b"]);
    assert!(!on_trunk("a"), "upstack of b leaves a where it was");

    assert!(
        sb.ok(&["restack", "--no-trunk"])
            .contains("already in place")
    );
    assert!(
        !on_trunk("a") && !on_trunk("d"),
        "--no-trunk leaves trunk-based changes alone"
    );

    let out = sb.ok(&["restack", "c", "--downstack"]);
    assert!(out.contains("restacked a, b, c"), "{out}");
    assert!(
        on_trunk("c") && !on_trunk("d"),
        "downstack of c doesn't touch d"
    );

    let (ok, out) = sb.dags(&["restack", "c"]);
    assert!(!ok && out.contains("--upstack or --downstack"), "{out}");
    assert!(sb.ok(&["restack"]).contains("restacked d"));
}

#[test]
fn completion_offers_names_from_the_repo() {
    let sb = Sandbox::new();
    forked(&sb);
    let out = sb.ok(&["__complete", "--", "switch", ""]);
    assert!(
        out.contains("theme\ton auth\n") && out.contains("main\ttrunk\n"),
        "{out}"
    );
    let out = sb.ok(&["__complete", "--", ""]);
    assert!(
        out.contains("up\t") && out.contains("restack\t") && !out.contains("__complete"),
        "{out}"
    );

    // Outside a repository there's nothing to offer, but no error either.
    let out = command(
        env!("CARGO_BIN_EXE_gh-dags"),
        sb._tmp.path(),
        sb._tmp.path(),
    )
    .args(["__complete", "--", "switch", ""])
    .output()
    .unwrap();
    assert!(out.status.success() && out.stdout.is_empty());
}

#[test]
fn fish_completion_survives_ghs_own_script() {
    if !has("fish") {
        eprintln!("fish isn't installed; skipping");
        return;
    }
    let sb = Sandbox::new();
    forked(&sb);
    // Installed the documented way: as fish's gh.fish, which gh's script
    // (erasing every gh rule first) shares with ours.
    let script = sb.ok(&["completion", "fish"]);
    let dir = sb._tmp.path().join("config/fish/completions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("gh.fish"), script).unwrap();

    let out = sb.shell("fish", "complete -C 'gh dags switch '");
    assert!(
        out.contains("theme\ton auth") && out.contains("login\ton theme"),
        "{out}"
    );
    let out = sb.shell("fish", "complete -C 'gh dags new x --on auth,'");
    assert!(out.contains("auth,theme"), "{out}");
    let out = sb.shell("fish", "complete -C 'gh '");
    assert!(
        out.contains("pr") && !out.contains("theme"),
        "gh's own completion still works: {out}"
    );
}

#[test]
fn bash_completion_hands_other_commands_to_gh() {
    if !has("bash") {
        return;
    }
    let sb = Sandbox::new();
    forked(&sb);
    let script = sb.ok(&["completion", "bash"]);
    let run = |words: &str, cword: usize| {
        sb.shell(
            "bash",
            &format!(
                "{script}\nCOMP_WORDS=({words}); COMP_CWORD={cword}; _gh_dags_wrap; printf '%s\\n' \"${{COMPREPLY[@]}}\""
            ),
        )
    };
    let out = run("gh dags switch th", 3);
    assert_eq!(out.trim(), "theme");
    let out = run("gh pr ''", 2);
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["pr", "repo"]);
}

#[test]
fn zsh_completion_script_parses() {
    if !has("zsh") {
        return;
    }
    let sb = Sandbox::new();
    let script = sb.ok(&["completion", "zsh"]);
    let path = sb._tmp.path().join("gh-dags.zsh");
    fs::write(&path, script).unwrap();
    let out = Command::new("zsh").arg("-n").arg(&path).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A change without a PR yet, linked in under a stacked PR: submit has to
/// free the stacked PR before retargeting it. (Found while formalizing the
/// retarget order in Lean: the pre-push plan only looked at stacks up to their
/// first PR-less change, so it missed this one.)
#[test]
fn submit_frees_a_stacked_pr_before_retargeting_it() {
    let sb = Sandbox::new();
    sb.ok(&["new", "m"]);
    sb.commit_file("m", "x\n", "m");
    sb.ok(&["new", "n"]);
    sb.commit_file("n", "x\n", "n");
    sb.ok(&["submit"]);
    assert_eq!(sb.stack(1), [1, 2]);

    sb.git(&["checkout", "-q", "main"]);
    sb.ok(&["new", "x"]);
    sb.commit_file("x", "x\n", "x");
    sb.ok(&["link", "m", "x"]);
    let out = sb.ok(&["submit"]);
    assert!(out.contains("retargeted #1 m → x"), "{out}");
    assert_eq!(sb.pr_base(1), "x");
    assert_eq!(sb.stack(2), [3, 1, 2]);
    assert!(sb.github().get("implicitly_merged").is_none());
}
