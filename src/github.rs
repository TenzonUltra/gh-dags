//! GitHub access through `gh api`, so auth, hosts (GHES), and the current
//! repository all come from `gh` itself.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub API error {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// The HTTP status of a failed API call, if that's what the error was.
pub fn status_of(err: &anyhow::Error) -> Option<u16> {
    err.downcast_ref::<ApiError>().map(|e| e.status)
}

pub struct Gh {
    dir: PathBuf,
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    /// The GraphQL node id.
    pub id: String,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    pub draft: bool,
    pub base: String,
    pub body: String,
    pub url: String,
    /// The commit a merged PR became (a squash, merge or the last rebased
    /// commit), wherever it landed.
    pub merge_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteStack {
    pub number: u64,
    pub base: String,
    /// Bottom to top: (PR number, still open).
    pub prs: Vec<(u64, bool)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStatus {
    Pending(String),
    Merged,
    Enqueued,
    Failed(String),
}

impl Gh {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Gh> {
        let dir = dir.into();
        let out = Command::new("gh")
            .current_dir(&dir)
            .args([
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "--jq",
                ".nameWithOwner",
            ])
            .output()
            .context("running gh (is the GitHub CLI installed?)")?;
        if !out.status.success() {
            bail!(
                "couldn't find the GitHub repository: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let (owner, name) = full
            .split_once('/')
            .with_context(|| format!("unexpected repository name {full:?}"))?;
        Ok(Gh {
            dir,
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    fn repo_path(&self, rest: &str) -> String {
        format!("repos/{}/{}/{rest}", self.owner, self.name)
    }

    /// One REST call. 204 responses come back as `Value::Null`.
    pub fn api(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut cmd = Command::new("gh");
        cmd.current_dir(&self.dir)
            .args(["api", "--method", method, path])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if body.is_some() {
            cmd.args(["--input", "-"]).stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        let mut child = cmd.spawn().context("running gh api")?;
        if let Some(b) = body {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b.to_string().as_bytes())?;
        }
        let out = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            // gh ends with "gh: <message> (HTTP 422)", or just "gh: HTTP 400".
            let stderr = String::from_utf8_lossy(&out.stderr);
            let status = stderr
                .rsplit_once("HTTP ")
                .and_then(|(_, rest)| rest.get(..3))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let message = serde_json::from_str::<Value>(&stdout)
                .ok()
                .and_then(|v| describe_error(&v))
                .unwrap_or_else(|| stderr.trim().to_string());
            return Err(ApiError { status, message }.into());
        }
        if stdout.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&stdout).with_context(|| format!("parsing the response from {path}"))
    }

    pub fn graphql(&self, query: &str) -> Result<Value> {
        let out = Command::new("gh")
            .current_dir(&self.dir)
            .args(["api", "graphql", "-f", &format!("query={query}")])
            .output()
            .context("running gh api graphql")?;
        if !out.status.success() {
            bail!(
                "GitHub GraphQL error: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        if let Some(errs) = v.get("errors") {
            bail!("GitHub GraphQL error: {errs}");
        }
        Ok(v)
    }

    /// State of several PRs in one round trip.
    pub fn prs(&self, numbers: &[u64]) -> Result<BTreeMap<u64, PrInfo>> {
        let mut out = BTreeMap::new();
        for chunk in numbers.chunks(50) {
            let fields: String = chunk
                .iter()
                .map(|n| {
                    format!(
                        "p{n}: pullRequest(number: {n}) {{ id number state isDraft baseRefName body url mergeCommit {{ oid }} }} "
                    )
                })
                .collect();
            let q = format!(
                "query {{ repository(owner: {}, name: {}) {{ {fields} }} }}",
                json!(self.owner),
                json!(self.name)
            );
            let v = self.graphql(&q)?;
            for n in chunk {
                let p = &v["data"]["repository"][format!("p{n}")];
                if p.is_null() {
                    continue;
                }
                out.insert(
                    *n,
                    PrInfo {
                        number: *n,
                        id: p["id"].as_str().unwrap_or_default().to_string(),
                        state: p["state"].as_str().unwrap_or_default().to_string(),
                        draft: p["isDraft"].as_bool().unwrap_or(false),
                        base: p["baseRefName"].as_str().unwrap_or_default().to_string(),
                        body: p["body"].as_str().unwrap_or_default().to_string(),
                        url: p["url"].as_str().unwrap_or_default().to_string(),
                        merge_commit: p["mergeCommit"]["oid"].as_str().map(String::from),
                    },
                );
            }
        }
        Ok(out)
    }

    /// Drafts can't be merged. REST can't un-draft a PR; GraphQL can.
    pub fn mark_ready(&self, id: &str) -> Result<()> {
        let q = format!(
            "mutation {{ markPullRequestReadyForReview(input: {{pullRequestId: {}}}) {{ clientMutationId }} }}",
            json!(id)
        );
        self.graphql(&q)?;
        Ok(())
    }

    pub fn open_pr_for(&self, branch: &str) -> Result<Option<u64>> {
        let path = self.repo_path(&format!("pulls?state=open&head={}:{branch}", self.owner));
        let v = self.api("GET", &path, None)?;
        Ok(v.as_array()
            .and_then(|a| a.first())
            .and_then(|p| p["number"].as_u64()))
    }

    pub fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<(u64, String)> {
        let v = self.api(
            "POST",
            &self.repo_path("pulls"),
            Some(
                &json!({"head": head, "base": base, "title": title, "body": body, "draft": draft}),
            ),
        )?;
        let number = v["number"]
            .as_u64()
            .ok_or_else(|| anyhow!("no PR number in response"))?;
        Ok((
            number,
            v["html_url"].as_str().unwrap_or_default().to_string(),
        ))
    }

    pub fn update_pr(&self, number: u64, fields: Value) -> Result<()> {
        self.api(
            "PATCH",
            &self.repo_path(&format!("pulls/{number}")),
            Some(&fields),
        )?;
        Ok(())
    }

    /// Every open stack in the repository.
    pub fn stacks(&self) -> Result<Vec<RemoteStack>> {
        #[derive(Deserialize)]
        struct S {
            number: u64,
            base: Ref,
            open: bool,
            pull_requests: Vec<P>,
        }
        #[derive(Deserialize)]
        struct Ref {
            #[serde(rename = "ref")]
            name: String,
        }
        #[derive(Deserialize)]
        struct P {
            number: u64,
            state: String,
        }
        let mut out = Vec::new();
        for page in 1.. {
            let v = match self.api(
                "GET",
                &self.repo_path(&format!("stacks?per_page=100&page={page}")),
                None,
            ) {
                Ok(v) => v,
                // No stacks (or stacks not enabled) reads as an empty list.
                Err(e) if status_of(&e) == Some(404) => break,
                Err(e) => return Err(e),
            };
            let batch: Vec<S> = serde_json::from_value(v)?;
            let done = batch.len() < 100;
            out.extend(batch.into_iter().filter(|s| s.open).map(|s| {
                RemoteStack {
                    number: s.number,
                    base: s.base.name,
                    prs: s
                        .pull_requests
                        .into_iter()
                        .map(|p| (p.number, p.state == "open"))
                        .collect(),
                }
            }));
            if done {
                break;
            }
        }
        Ok(out)
    }

    pub fn create_stack(&self, prs: &[u64]) -> Result<u64> {
        let v = self.api(
            "POST",
            &self.repo_path("stacks"),
            Some(&json!({"pull_requests": prs})),
        )?;
        v["number"]
            .as_u64()
            .ok_or_else(|| anyhow!("no stack number in response"))
    }

    pub fn add_to_stack(&self, number: u64, prs: &[u64]) -> Result<()> {
        let path = self.repo_path(&format!("stacks/{number}/add"));
        self.api("POST", &path, Some(&json!({"pull_requests": prs})))?;
        Ok(())
    }

    /// Release a stack's unmerged PRs. A stack that's already gone is fine.
    pub fn unstack(&self, number: u64) -> Result<()> {
        match self.api(
            "POST",
            &self.repo_path(&format!("stacks/{number}/unstack")),
            None,
        ) {
            Err(e) if status_of(&e) == Some(404) => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// Merge a PR (and, for a stacked PR, everything below it).
    pub fn merge(&self, number: u64, method: Option<&str>) -> Result<MergeStatus> {
        let mut body = json!({"merge_action": "default"});
        if let Some(m) = method {
            body["merge_method"] = json!(m);
        }
        let path = self.repo_path(&format!("pulls/{number}/merge-async"));
        match self.api("PUT", &path, Some(&body)) {
            Ok(v) => Ok(merge_status(&v)),
            // No async merge here: fall back to the classic endpoint.
            Err(e) if status_of(&e) == Some(404) => {
                let mut body = json!({});
                if let Some(m) = method {
                    body["merge_method"] = json!(m);
                }
                self.api(
                    "PUT",
                    &self.repo_path(&format!("pulls/{number}/merge")),
                    Some(&body),
                )?;
                Ok(MergeStatus::Merged)
            }
            Err(e) => Err(e),
        }
    }

    pub fn merge_progress(&self, number: u64, uuid: &str) -> Result<MergeStatus> {
        let v = self.api(
            "GET",
            &self.repo_path(&format!("pulls/{number}/merge-async/{uuid}")),
            None,
        )?;
        Ok(merge_status(&v))
    }
}

fn merge_status(v: &Value) -> MergeStatus {
    let details = &v["details"];
    match v["status"].as_str().unwrap_or_default() {
        "merged" => MergeStatus::Merged,
        "enqueued" => MergeStatus::Enqueued,
        "pending" => MergeStatus::Pending(details["uuid"].as_str().unwrap_or_default().to_string()),
        other => MergeStatus::Failed(
            details["message"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("merge status {other:?}")),
        ),
    }
}

fn describe_error(v: &Value) -> Option<String> {
    // Most errors carry "message"; the async merge API nests it in "details".
    let msg = v["message"]
        .as_str()
        .or(v["details"]["message"].as_str())?
        .to_string();
    let details: Vec<String> = v["errors"]
        .as_array()
        .map(|errs| {
            errs.iter()
                .filter_map(|e| e["message"].as_str().or(e.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Some(if details.is_empty() {
        msg
    } else {
        format!("{msg}: {}", details.join("; "))
    })
}
