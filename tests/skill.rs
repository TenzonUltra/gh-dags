//! The Claude skill can't drift from the tool: every `gh dags` command and
//! flag it mentions exists, and the plugin's version is the crate's.

use std::fs;
use std::path::Path;

use gh_dags::cli::command;

fn skill_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin/skills/gh-dags")
}

/// Each `gh dags …` in the text, as its words up to the closing backtick.
fn mentions(text: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some(i) = rest.find("gh dags ") {
            rest = &rest[i + "gh dags ".len()..];
            let end = rest.find('`').unwrap_or(rest.len());
            out.push(rest[..end].split_whitespace().map(String::from).collect());
        }
    }
    out
}

#[test]
fn the_skill_only_mentions_real_commands_and_flags() {
    let cli = command();
    let mut checked = 0;
    for file in ["SKILL.md", "reference.md"] {
        let text = fs::read_to_string(skill_dir().join(file)).unwrap();
        for words in mentions(&text) {
            let Some(name) = words.first() else { continue };
            // `gh dags <command> --help` and the like name no command.
            if !name.chars().all(|c| c.is_ascii_lowercase()) {
                continue;
            }
            let sub = cli
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("{file} mentions `gh dags {name}`, which doesn't exist"));
            for word in &words[1..] {
                for part in word.split('|') {
                    let flag = part.trim_matches(|c: char| "[]()\\,.".contains(c));
                    let Some(long) = flag.strip_prefix("--") else {
                        continue;
                    };
                    let long = long.split('=').next().unwrap();
                    let known =
                        long == "help" || sub.get_arguments().any(|a| a.get_long() == Some(long));
                    assert!(
                        known,
                        "{file} mentions `gh dags {name} --{long}`, which doesn't exist"
                    );
                    checked += 1;
                }
                if let Some(short) = word.strip_prefix('-').filter(|w| w.len() == 1) {
                    let c = short.chars().next().unwrap();
                    assert!(
                        sub.get_arguments().any(|a| a.get_short() == Some(c)),
                        "{file} mentions `gh dags {name} -{c}`, which doesn't exist"
                    );
                }
            }
            checked += 1;
        }
    }
    assert!(
        checked > 40,
        "only {checked} mentions found; is the parsing broken?"
    );
}

#[test]
fn the_plugin_version_is_the_crates() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin/.claude-plugin/plugin.json");
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(manifest).unwrap()).unwrap();
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
}
