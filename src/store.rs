//! Where state lives: `<git-common-dir>/gh-dags/`, shared by all worktrees and
//! never committed. Writes are atomic (temp file + rename), and commands that
//! change anything run one at a time, under `lock`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::git::Git;
use crate::model::{STATE_VERSION, State};

pub struct Repo {
    pub git: Git,
    dir: PathBuf,
}

/// The lock one gh-dags command holds while it runs. The OS releases it if
/// the process dies, so a crash can't leave it behind.
pub struct Lock(fs::File);

impl Drop for Lock {
    fn drop(&mut self) {
        // Clear who held it; the lock itself goes with the file handle.
        let _ = self.0.set_len(0);
    }
}

impl Repo {
    /// Wait for any other gh-dags command in this repository (from any
    /// worktree) to finish, then hold the lock until the returned value is
    /// dropped. `what` is the command, to tell anyone who has to wait.
    pub fn lock(&self, what: &str) -> Result<Lock> {
        fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("lock");
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let wait = std::env::var("GH_DAGS_LOCK_WAIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let deadline = Instant::now() + Duration::from_secs(wait);
        let mut said = false;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) => {
                    let holder = fs::read_to_string(&path).unwrap_or_default();
                    let holder = match holder.trim() {
                        "" => "another gh-dags command",
                        h => h,
                    }
                    .to_string();
                    if Instant::now() >= deadline {
                        bail!("{holder} is still running; try again once it's done");
                    }
                    if !said {
                        eprintln!("waiting for {holder} to finish…");
                        said = true;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
        let here = self.git.toplevel().unwrap_or_default();
        file.set_len(0)?;
        write!(
            file,
            "`{what}` (pid {}, in {})",
            std::process::id(),
            here.display()
        )?;
        Ok(Lock(file))
    }

    pub fn open(cwd: impl Into<PathBuf>) -> Result<Repo> {
        crate::git::check_version()?;
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
