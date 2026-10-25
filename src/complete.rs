//! Tab completion. `gh` doesn't pass completion through to extensions (it
//! registers them without a completion hook), so `gh dags completion <shell>`
//! prints a snippet that takes over when the second word is `dags`, and asks
//! the hidden `gh dags __complete <words…>` for candidates. Candidates come
//! from local state only, so completion is quick and works offline.

use clap::{Command, ValueEnum};

use crate::model::State;
use crate::store::Repo;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Shell {
    Fish,
    Zsh,
    Bash,
}

/// What completion can look at. Everything is optional: outside a repo (or
/// before `init`) there are just no names to offer.
pub struct Context {
    pub state: Option<State>,
    pub branches: Vec<String>,
    pub remotes: Vec<String>,
}

impl Context {
    pub fn load(repo: Option<&Repo>) -> Context {
        let lines = |args: &[&str]| -> Vec<String> {
            repo.and_then(|r| r.git.run(args).ok())
                .map(|s| s.lines().map(String::from).collect())
                .unwrap_or_default()
        };
        Context {
            state: repo.and_then(|r| r.load().ok()),
            branches: lines(&["for-each-ref", "--format=%(refname:short)", "refs/heads"]),
            remotes: lines(&["remote"]),
        }
    }
}

/// Flags whose value is the next word.
fn takes_value(cmd: &Command, flag: &str) -> bool {
    cmd.get_arguments().any(|a| {
        let named = flag
            .strip_prefix("--")
            .map(|l| a.get_long() == Some(l))
            .or_else(|| {
                flag.strip_prefix('-')
                    .map(|s| a.get_short().map(String::from).as_deref() == Some(s))
            })
            .unwrap_or(false);
        named && a.get_action().takes_values()
    })
}

/// Candidates, as (value, description), for `words`: everything after
/// `dags`, the last being the word under the cursor (possibly empty).
pub fn candidates(root: &Command, ctx: &Context, words: &[String]) -> Vec<(String, String)> {
    let (cur, before) = match words.split_last() {
        Some((cur, before)) => (cur.as_str(), before),
        None => ("", &[][..]),
    };
    let Some(i) = before.iter().position(|w| !w.starts_with('-')) else {
        if cur.starts_with('-') {
            return flags(root);
        }
        return root
            .get_subcommands()
            .filter(|s| !s.is_hide_set() && s.get_name() != "help")
            .map(|s| (s.get_name().to_string(), about(s)))
            .collect();
    };
    let Some(sub) = root.find_subcommand(&before[i]) else {
        return vec![];
    };
    let args = &before[i + 1..];

    if let Some(prev) = args.last()
        && takes_value(sub, prev)
    {
        return values(ctx, prev, "", cur);
    }
    if let Some((flag, partial)) = cur.split_once('=')
        && flag.starts_with("--")
    {
        return values(ctx, flag, &format!("{flag}="), partial);
    }
    if cur.starts_with('-') {
        return flags(sub);
    }

    // Positionals typed so far, skipping flags and their values.
    let mut positionals = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
        } else if a.starts_with('-') {
            skip = !a.contains('=') && takes_value(sub, a);
        } else {
            positionals.push(a.as_str());
        }
    }
    positional(ctx, sub.get_name(), &positionals)
}

fn about(cmd: &Command) -> String {
    cmd.get_about().map(|a| a.to_string()).unwrap_or_default()
}

fn flags(cmd: &Command) -> Vec<(String, String)> {
    cmd.get_arguments()
        .filter(|a| !a.is_hide_set())
        .filter_map(|a| {
            let help = a.get_help().map(|h| h.to_string()).unwrap_or_default();
            a.get_long().map(|l| (format!("--{l}"), help))
        })
        .collect()
}

/// Tracked changes, described by where they sit.
fn changes(ctx: &Context) -> Vec<(String, String)> {
    let Some(st) = &ctx.state else { return vec![] };
    st.changes
        .iter()
        .map(|(name, c)| {
            let on = st
                .parent(name)
                .map(|b| crate::layout::describe_base(st, &b))
                .unwrap_or_default();
            let desc = match c.pr {
                Some(n) => format!("on {on} · #{n}"),
                None => format!("on {on}"),
            };
            (name.clone(), desc)
        })
        .collect()
}

fn with_trunk(ctx: &Context, mut list: Vec<(String, String)>) -> Vec<(String, String)> {
    if let Some(st) = &ctx.state {
        list.push((st.trunk.clone(), "trunk".into()));
    }
    list
}

fn values(ctx: &Context, flag: &str, prefix: &str, partial: &str) -> Vec<(String, String)> {
    let list: Vec<(String, String)> = match flag {
        "--on" => {
            // A comma-separated list: complete the last item, keep the rest.
            let (done, _) = partial.rsplit_once(',').unwrap_or(("", partial));
            let taken: Vec<&str> = done.split(',').collect();
            let lead = if done.is_empty() {
                String::new()
            } else {
                format!("{done},")
            };
            return with_trunk(ctx, changes(ctx))
                .into_iter()
                .filter(|(n, _)| !taken.contains(&n.as_str()))
                .map(|(n, d)| (format!("{prefix}{lead}{n}"), d))
                .collect();
        }
        "--onto" => with_trunk(ctx, changes(ctx)),
        "--method" => ["squash", "merge", "rebase"]
            .map(|m| (m.to_string(), String::new()))
            .to_vec(),
        "--trunk" => ctx
            .branches
            .iter()
            .map(|b| (b.clone(), String::new()))
            .collect(),
        "--remote" => ctx
            .remotes
            .iter()
            .map(|r| (r.clone(), String::new()))
            .collect(),
        _ => vec![],
    };
    list.into_iter()
        .map(|(v, d)| (format!("{prefix}{v}"), d))
        .collect()
}

fn positional(ctx: &Context, sub: &str, typed: &[&str]) -> Vec<(String, String)> {
    let first = typed.is_empty();
    match sub {
        "switch" if first => with_trunk(ctx, changes(ctx)),
        "untrack" | "land" | "move" | "restack" if first => changes(ctx),
        "link" => changes(ctx)
            .into_iter()
            .filter(|(n, _)| !typed.contains(&n.as_str()))
            .collect(),
        "unlink" if first => changes(ctx),
        "unlink" => {
            let Some(st) = &ctx.state else { return vec![] };
            st.changes
                .get(typed[0])
                .map(|c| {
                    c.deps
                        .iter()
                        .map(|d| (d.clone(), "dependency".into()))
                        .collect()
                })
                .unwrap_or_default()
        }
        "track" if first => {
            let tracked = |b: &String| {
                ctx.state
                    .as_ref()
                    .is_some_and(|st| st.has(b) || *b == st.trunk)
            };
            ctx.branches
                .iter()
                .filter(|b| !tracked(b))
                .map(|b| (b.clone(), "untracked branch".into()))
                .collect()
        }
        "completion" if first => Shell::value_variants()
            .iter()
            .filter_map(|s| s.to_possible_value())
            .map(|v| (v.get_name().to_string(), String::new()))
            .collect(),
        _ => vec![],
    }
}

/// The shell code that hooks `gh dags` into `gh`'s completion.
pub fn script(shell: Shell, gh_script: &str) -> String {
    match shell {
        // Loading gh's fish completions erases every `gh` rule first, so ours
        // can't live in config.fish; they ship in the same file, after gh's.
        Shell::Fish => format!(
            "# gh's own completions, then gh dags'. Save this as\n\
             # ~/.config/fish/completions/gh.fish\n\
             {gh_script}\n\n\
             {FISH}"
        ),
        Shell::Zsh => ZSH.to_string(),
        Shell::Bash => BASH.to_string(),
    }
}

const FISH: &str = r#"# --- gh dags ---
function __gh_dags_tokens
    commandline -xpc 2>/dev/null; or commandline -opc
end
function __gh_dags_active
    set -l tokens (__gh_dags_tokens)
    test (count $tokens) -ge 2; and test "$tokens[2]" = dags
end
function __gh_dags_candidates
    set -l tokens (__gh_dags_tokens)
    set -l rest
    if test (count $tokens) -gt 2
        set rest $tokens[3..-1]
    end
    gh dags __complete -- $rest (commandline -ct) 2>/dev/null
end
complete -c gh -n __gh_dags_active -f -a '(__gh_dags_candidates)'
"#;

const ZSH: &str = r#"# gh dags completion. In ~/.zshrc, after compinit:
#   eval "$(gh dags completion zsh)"
_gh_dags_wrap() {
  if [[ ${words[2]} != dags ]]; then
    _gh "$@"
    return
  fi
  local -a entries
  local line value desc
  for line in "${(@f)$(gh dags __complete -- "${(@)words[3,CURRENT]}" 2>/dev/null)}"; do
    [[ -n $line ]] || continue
    value=${line%%$'\t'*}
    desc=${line#*$'\t'}
    [[ $line == *$'\t'* ]] || desc=
    entries+=("${value//:/\\:}:$desc")
  done
  _describe -t gh-dags 'gh dags' entries
}
compdef _gh_dags_wrap gh
"#;

const BASH: &str = r#"# gh dags completion. In ~/.bashrc:
#   eval "$(gh dags completion bash)"
# gh's completion has to be loaded first, since ours hands everything else to it.
if ! declare -F __start_gh >/dev/null 2>&1; then
  eval "$(gh completion -s bash 2>/dev/null)"
fi
_gh_dags_wrap() {
  if [[ ${COMP_WORDS[1]} != dags ]]; then
    __start_gh "$@"
    return
  fi
  local cur=${COMP_WORDS[COMP_CWORD]}
  local IFS=$'\n'
  COMPREPLY=($(compgen -W "$(gh dags __complete -- "${COMP_WORDS[@]:2:COMP_CWORD-2}" "$cur" 2>/dev/null | cut -f1)" -- "$cur"))
}
complete -o default -F _gh_dags_wrap gh
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{FanIn, add_change};

    fn ctx() -> Context {
        let mut st = State::new("main", "origin");
        for (n, d) in [
            ("auth", vec![]),
            ("theme", vec![]),
            ("login", vec!["auth", "theme"]),
        ] {
            let d: Vec<String> = d.into_iter().map(String::from).collect();
            add_change(&mut st, n, &d, FanIn::Linearize).unwrap();
        }
        st.changes.get_mut("auth").unwrap().pr = Some(12);
        Context {
            state: Some(st),
            branches: vec![
                "main".into(),
                "auth".into(),
                "theme".into(),
                "login".into(),
                "wip".into(),
            ],
            remotes: vec!["origin".into()],
        }
    }

    fn values_for(words: &[&str]) -> Vec<String> {
        let words: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        candidates(&crate::cli::command(), &ctx(), &words)
            .into_iter()
            .map(|(v, _)| v)
            .collect()
    }

    #[test]
    fn subcommands_then_names_then_flags() {
        let subs = values_for(&[""]);
        assert!(
            subs.contains(&"switch".to_string()) && subs.contains(&"up".to_string()),
            "{subs:?}"
        );
        assert!(
            !subs.iter().any(|s| s.starts_with("__")),
            "hidden commands stay hidden"
        );
        assert_eq!(
            values_for(&["switch", ""]),
            ["auth", "login", "theme", "main"]
        );
        assert!(values_for(&["restack", "--"]).contains(&"--upstack".to_string()));
        assert_eq!(
            values_for(&["land", "auth", "--method", ""]),
            ["squash", "merge", "rebase"]
        );
    }

    #[test]
    fn context_specific_values() {
        assert_eq!(values_for(&["unlink", "login", ""]), ["auth", "theme"]);
        assert_eq!(values_for(&["track", ""]), ["wip"]);
        assert_eq!(values_for(&["link", "login", "auth", ""]), ["theme"]);
        assert_eq!(
            values_for(&["new", "x", "--on", "auth,"]),
            ["auth,login", "auth,theme", "auth,main"]
        );
        assert_eq!(
            values_for(&["move", "x", "--onto=th"])[..2],
            ["--onto=auth", "--onto=login"]
        );
        assert!(
            values_for(&["new", ""]).is_empty(),
            "a new name can't be completed"
        );
    }
}
