//! A thin wrapper over the git CLI. Everything gh-dags does to a repository
//! goes through here.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub struct Git {
    dir: PathBuf,
}

pub struct Output {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Result of replaying a change's own commits onto a new base.
pub enum Replay {
    /// The new tip.
    Done(String),
    /// The range had no commits.
    Empty,
    /// Conflict, merge commits, or anything else replay can't do in memory.
    Failed,
}

/// Fixed identity for synthetic sum commits, so identical inputs always give
/// an identical commit.
const SUM_IDENT: &str = "gh-dags";
const SUM_EMAIL: &str = "gh-dags@localhost";

impl Git {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Git { dir: dir.into() }
    }

    pub fn exec(
        &self,
        args: &[&str],
        stdin: Option<&str>,
        env: &[(&str, String)],
    ) -> Result<Output> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.dir).args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("running git")?;
        if let Some(input) = stdin {
            child.stdin.take().unwrap().write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        Ok(Output {
            ok: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
        })
    }

    /// Run git and return trimmed stdout, failing with git's stderr.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.exec(args, None, &[])?;
        if !out.ok {
            bail!("git {} failed: {}", args.join(" "), out.stderr);
        }
        Ok(out.stdout)
    }

    /// Run git with the terminal attached, for commands the user may need to
    /// see (rebase conflicts, push progress). Returns whether it succeeded.
    pub fn run_visible(&self, args: &[&str], env: &[(&str, &str)]) -> Result<bool> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.dir).args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        Ok(cmd.status().context("running git")?.success())
    }

    pub fn ok(&self, args: &[&str]) -> bool {
        self.exec(args, None, &[]).is_ok_and(|o| o.ok)
    }

    pub fn resolve(&self, rev: &str) -> Option<String> {
        let spec = format!("{rev}^{{commit}}");
        self.exec(&["rev-parse", "--verify", "--quiet", &spec], None, &[])
            .ok()
            .filter(|o| o.ok)
            .map(|o| o.stdout)
    }

    pub fn branch_tip(&self, branch: &str) -> Result<String> {
        self.resolve(&format!("refs/heads/{branch}"))
            .with_context(|| format!("branch {branch:?} doesn't exist"))
    }

    pub fn branch_exists(&self, branch: &str) -> bool {
        self.resolve(&format!("refs/heads/{branch}")).is_some()
    }

    pub fn common_dir(&self) -> Result<PathBuf> {
        let dir = self.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        Ok(PathBuf::from(dir))
    }

    pub fn toplevel(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(self.run(&["rev-parse", "--show-toplevel"])?))
    }

    pub fn current_branch(&self) -> Option<String> {
        self.exec(&["symbolic-ref", "--quiet", "--short", "HEAD"], None, &[])
            .ok()
            .filter(|o| o.ok)
            .map(|o| o.stdout)
    }

    /// The branch name if on one, else the detached commit.
    pub fn head(&self) -> Result<String> {
        match self.current_branch() {
            Some(b) => Ok(b),
            None => self
                .resolve("HEAD")
                .context("HEAD doesn't point at a commit"),
        }
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        self.exec(&["merge-base", a, b], None, &[])
            .ok()
            .filter(|o| o.ok)
            .map(|o| o.stdout)
    }

    pub fn is_ancestor(&self, ancestor: &str, of: &str) -> bool {
        self.ok(&["merge-base", "--is-ancestor", ancestor, of])
    }

    /// No staged or unstaged changes to tracked files.
    pub fn is_clean(&self) -> Result<bool> {
        Ok(self
            .run(&["status", "--porcelain", "--untracked-files=no"])?
            .is_empty())
    }

    pub fn rebase_in_progress(&self) -> bool {
        ["rebase-merge", "rebase-apply"].iter().any(|d| {
            self.run(&["rev-parse", "--path-format=absolute", "--git-path", d])
                .is_ok_and(|p| Path::new(&p).exists())
        })
    }

    /// Branch → worktree path, for every branch checked out somewhere.
    pub fn worktree_branches(&self) -> Result<BTreeMap<String, PathBuf>> {
        let out = self.run(&["worktree", "list", "--porcelain"])?;
        let mut map = BTreeMap::new();
        let mut path = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/")
                && let Some(p) = &path
            {
                map.insert(b.to_string(), p.clone());
            }
        }
        Ok(map)
    }

    /// Merge two commits in memory. `None` means they conflict.
    pub fn merge_tree(&self, a: &str, b: &str) -> Result<Option<String>> {
        let out = self.exec(&["merge-tree", "--write-tree", a, b], None, &[])?;
        match out.code {
            Some(0) => Ok(out.stdout.lines().next().map(str::to_string)),
            Some(1) => Ok(None),
            _ => bail!("git merge-tree failed: {}", out.stderr),
        }
    }

    pub fn committer_time(&self, rev: &str) -> Result<i64> {
        Ok(self.run(&["log", "-1", "--format=%ct", rev])?.parse()?)
    }

    /// A commit with a fixed identity and the given time, so it is
    /// reproducible from its inputs.
    pub fn commit_tree(
        &self,
        tree: &str,
        parents: &[String],
        msg: &str,
        time: i64,
    ) -> Result<String> {
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.extend(["-p", p.as_str()]);
        }
        args.extend(["-m", msg]);
        let date = format!("@{time} +0000");
        let env = [
            ("GIT_AUTHOR_NAME", SUM_IDENT.to_string()),
            ("GIT_AUTHOR_EMAIL", SUM_EMAIL.to_string()),
            ("GIT_AUTHOR_DATE", date.clone()),
            ("GIT_COMMITTER_NAME", SUM_IDENT.to_string()),
            ("GIT_COMMITTER_EMAIL", SUM_EMAIL.to_string()),
            ("GIT_COMMITTER_DATE", date),
        ];
        let out = self.exec(&args, None, &env)?;
        if !out.ok {
            bail!("git commit-tree failed: {}", out.stderr);
        }
        Ok(out.stdout)
    }

    /// Replay `fork..branch` onto `onto` without touching the worktree.
    pub fn replay(&self, onto: &str, fork: &str, branch: &str) -> Result<Replay> {
        let full = format!("refs/heads/{branch}");
        let range = format!("{fork}..{full}");
        let onto_arg = format!("--onto={onto}");
        let out = self.exec(
            &["replay", "--ref-action=print", &onto_arg, &range],
            None,
            &[],
        )?;
        if !out.ok {
            return Ok(Replay::Failed);
        }
        for line in out.stdout.lines() {
            let mut parts = line.split_whitespace();
            if let (Some("update"), Some(r), Some(new)) = (parts.next(), parts.next(), parts.next())
                && r == full
            {
                return Ok(Replay::Done(new.to_string()));
            }
        }
        Ok(Replay::Empty)
    }

    /// Move a branch, failing if it isn't at `old` any more.
    pub fn update_branch(&self, branch: &str, new: &str, old: &str) -> Result<()> {
        let full = format!("refs/heads/{branch}");
        self.run(&["update-ref", "-m", "gh-dags restack", &full, new, old])?;
        Ok(())
    }

    /// Oldest-first subjects of the commits in `from..to`.
    pub fn subjects(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let out = self.run(&["log", "--reverse", "--format=%s", &format!("{from}..{to}")])?;
        Ok(out.lines().map(String::from).collect())
    }

    /// Where the given branches are on the remote right now (absent if not there).
    pub fn remote_tips(
        &self,
        remote: &str,
        branches: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let refs: Vec<String> = branches.iter().map(|b| format!("refs/heads/{b}")).collect();
        let mut args = vec!["ls-remote", remote];
        args.extend(refs.iter().map(String::as_str));
        let out = self.run(&args)?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (sha, r) = l.split_once('\t')?;
                Some((r.strip_prefix("refs/heads/")?.to_string(), sha.to_string()))
            })
            .collect())
    }

    /// Push branches in one atomic push, each leased on where we expect it to
    /// be on the remote (empty: must not exist yet).
    pub fn push_leased(&self, remote: &str, leases: &[(String, String)]) -> Result<()> {
        let mut args: Vec<String> = vec![
            "push".into(),
            "--atomic".into(),
            "--quiet".into(),
            remote.into(),
        ];
        for (branch, expect) in leases {
            args.push(format!("--force-with-lease=refs/heads/{branch}:{expect}"));
        }
        for (branch, _) in leases {
            args.push(format!("refs/heads/{branch}:refs/heads/{branch}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args)?;
        Ok(())
    }
}
