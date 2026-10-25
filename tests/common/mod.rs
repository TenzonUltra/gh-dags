//! The test sandbox: a throwaway repository with a bare `origin`, isolated
//! from the user's git config, with the fake `gh` first on PATH.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

pub struct Sandbox {
    /// Shared by every clone of the same origin.
    pub _tmp: std::rc::Rc<TempDir>,
    pub work: PathBuf,
    /// Extra environment for gh-dags runs.
    pub env: Vec<(String, String)>,
}

/// `home` is the sandbox root: it holds the bare origin and the fake GitHub's
/// database, and stands in for $HOME.
pub fn command(program: &str, dir: &Path, home: &Path) -> Command {
    let fake_gh = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-gh");
    let path = format!(
        "{}:{}",
        fake_gh.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new(program);
    cmd.current_dir(dir)
        .env("PATH", path)
        .env("FAKE_GH_DB", home.join("github.json"))
        .env("FAKE_GH_ORIGIN", home.join("origin.git"))
        .env("FAKE_GH_DAGS", env!("CARGO_BIN_EXE_gh-dags"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("NO_COLOR", "1");
    cmd
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let tmp = TempDir::new().unwrap();
        let origin = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        let sb = Sandbox {
            work: work.clone(),
            _tmp: std::rc::Rc::new(tmp),
            env: Vec::new(),
        };
        sb.git_in(
            sb._tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        sb.git_in(
            sb._tmp.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        sb.commit_file("README", "hello\n", "initial");
        sb.git(&["push", "-q", "origin", "main"]);
        sb.ok(&["init"]);
        sb
    }

    /// Another clone of the same origin (and the same fake GitHub), set up
    /// with `gh dags init`.
    pub fn another_clone(&self, name: &str) -> Sandbox {
        let work = self._tmp.path().join(name);
        let origin = self._tmp.path().join("origin.git");
        self.git_in(
            self._tmp.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        let sb = Sandbox {
            _tmp: self._tmp.clone(),
            work,
            env: self.env.clone(),
        };
        sb.ok(&["init"]);
        sb
    }

    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = command("git", dir, self._tmp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.work, args)
    }

    pub fn dags(&self, args: &[&str]) -> (bool, String) {
        let out = command(env!("CARGO_BIN_EXE_gh-dags"), &self.work, self._tmp.path())
            .args(args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    pub fn ok(&self, args: &[&str]) -> String {
        let (ok, out) = self.dags(args);
        assert!(ok, "gh-dags {args:?} failed:\n{out}");
        out
    }

    pub fn commit_file(&self, path: &str, content: &str, msg: &str) {
        fs::write(self.work.join(path), content).unwrap();
        self.git(&["add", path]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    /// Write a file and stage it, without committing (resolving a conflict).
    pub fn commit_file_staged_only(&self, path: &str, content: &str) {
        fs::write(self.work.join(path), content).unwrap();
        self.git(&["add", path]);
    }

    pub fn branch(&self) -> String {
        self.git(&["branch", "--show-current"])
    }

    pub fn tip(&self, rev: &str) -> String {
        self.git(&["rev-parse", rev])
    }

    /// Subjects of the commits on `branch` that aren't on `base`, oldest first.
    pub fn own_commits(&self, base: &str, branch: &str) -> Vec<String> {
        let out = self.git(&[
            "log",
            "--reverse",
            "--format=%s",
            &format!("{base}..{branch}"),
        ]);
        out.lines().map(String::from).collect()
    }

    pub fn show(&self, rev: &str, path: &str) -> String {
        self.git(&["show", &format!("{rev}:{path}")])
    }

    pub fn parents(&self, rev: &str) -> usize {
        self.git(&["rev-list", "--parents", "-n1", rev])
            .split(' ')
            .count()
            - 1
    }

    /// The fake GitHub's database.
    pub fn github(&self) -> serde_json::Value {
        match fs::read_to_string(self._tmp.path().join("github.json")) {
            Ok(text) => serde_json::from_str(&text).unwrap(),
            // Nothing has talked to GitHub yet.
            Err(_) => serde_json::json!({"prs": {}, "stacks": {}, "calls": []}),
        }
    }

    pub fn pr_base(&self, n: u64) -> String {
        self.github()["prs"][n.to_string()]["base"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// PR numbers of an open GitHub stack, bottom to top (merged ones included).
    pub fn stack(&self, n: u64) -> Vec<u64> {
        let gh = self.github();
        let s = &gh["stacks"][n.to_string()];
        assert_eq!(s["open"], true, "stack #{n} is open");
        s["prs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect()
    }
}
