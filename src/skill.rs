//! The Claude skill for gh-dags. It's built into the binary, so
//! `gh dags skill install` always writes the copy that matches this version.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::git::Git;

const FILES: [(&str, &str); 2] = [
    (
        "SKILL.md",
        include_str!("../plugin/skills/gh-dags/SKILL.md"),
    ),
    (
        "reference.md",
        include_str!("../plugin/skills/gh-dags/reference.md"),
    ),
];

/// Write the skill into your Claude skills, or with `project`, into this
/// repository's.
pub fn install(project: bool) -> Result<()> {
    let root = if project {
        Git::new(std::env::current_dir()?)
            .toplevel()
            .context("--project installs into a git repository; run it inside one")?
            .join(".claude")
    } else if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        PathBuf::from(dir)
    } else {
        PathBuf::from(std::env::var_os("HOME").context("HOME isn't set")?).join(".claude")
    };
    let dir = root.join("skills").join("gh-dags");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, text) in FILES {
        fs::write(dir.join(name), text)?;
    }
    println!("installed the gh-dags skill in {}", dir.display());
    Ok(())
}
