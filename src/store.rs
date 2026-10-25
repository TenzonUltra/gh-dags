//! Where state lives: `<git-common-dir>/gh-dags/`, shared by all worktrees and
//! never committed. Writes are atomic (temp file + rename).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::git::Git;
use crate::model::{STATE_VERSION, State};

pub struct Repo {
    pub git: Git,
    dir: PathBuf,
}

impl Repo {
    pub fn open(cwd: impl Into<PathBuf>) -> Result<Repo> {
        let git = Git::new(cwd);
        let dir = git
            .common_dir()
            .context("not inside a git repository")?
            .join("gh-dags");
        Ok(Repo { git, dir })
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn journal_path(&self) -> PathBuf {
        self.dir.join("restack.json")
    }

    pub fn is_initialized(&self) -> bool {
        self.state_path().exists()
    }

    pub fn load(&self) -> Result<State> {
        if !self.is_initialized() {
            bail!("gh-dags isn't set up in this repository; run `gh dags init`");
        }
        let st: State = read_json(&self.state_path())?;
        if st.version != STATE_VERSION {
            bail!(
                "state file version {} isn't supported by this gh-dags",
                st.version
            );
        }
        Ok(st)
    }

    pub fn save(&self, st: &State) -> Result<()> {
        write_json(&self.state_path(), st)
    }

    pub fn load_journal<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let p = self.journal_path();
        if !p.exists() {
            return Ok(None);
        }
        read_json(&p).map(Some)
    }

    pub fn save_journal<T: Serialize>(&self, journal: &T) -> Result<()> {
        write_json(&self.journal_path(), journal)
    }

    pub fn clear_journal(&self) -> Result<()> {
        let p = self.journal_path();
        if p.exists() {
            fs::remove_file(p)?;
        }
        Ok(())
    }

    pub fn has_journal(&self) -> bool {
        self.journal_path().exists()
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path.parent().unwrap();
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
