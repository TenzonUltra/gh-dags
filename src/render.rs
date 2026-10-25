//! The `view` tree: the physical layout drawn from trunk outward. A stack's
//! continuation is the first child and spurs branch off beside it. Each line
//! shows the logical deps when they say more than the parent does, and
//! flags ordering-only edges.

use crate::layout::describe_base;
use crate::model::{Base, State};

pub struct Style {
    color: bool,
}

impl Style {
    pub fn new(color: bool) -> Self {
        Style { color }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
}

struct Line {
    /// Tree drawing before the name.
    prefix: String,
    name: String,
    current: bool,
    pr: String,
    notes: Vec<(bool, String)>,
}

pub fn render(st: &State, current: Option<&str>, style: &Style) -> String {
    rows(st, current, style)
        .into_iter()
        .map(|(_, text)| text + "\n")
        .collect()
}

/// The view as rows, each with the branch it shows (the trunk for the first
/// row, `None` for section headers), so a picker can number them.
pub fn rows(st: &State, current: Option<&str>, style: &Style) -> Vec<(Option<String>, String)> {
    let mut out = vec![(Some(st.trunk.clone()), style.bold(&st.trunk))];
    if st.changes.is_empty() {
        let hint = "  (no changes yet; create one with `gh dags new <name>`)";
        out.push((None, style.dim(hint)));
        return out;
    }
    // Sections: the trunk tree, then one per parked stack under its sum.
    let mut sections: Vec<(Option<String>, Vec<Line>)> = Vec::new();
    let mut lines = Vec::new();
    let roots = st.roots();
    for (i, root) in roots.iter().enumerate() {
        walk(st, root, "", i + 1 == roots.len(), current, &mut lines);
    }
    sections.push((None, lines));
    for s in st.stacks.iter().filter(|s| matches!(s.base, Base::Sum(_))) {
        let mut lines = Vec::new();
        walk(st, &s.changes[0], "", true, current, &mut lines);
        sections.push((
            Some(format!("parked on {}", describe_base(st, &s.base))),
            lines,
        ));
    }

    let all = || sections.iter().flat_map(|(_, ls)| ls);
    let width = all()
        .map(|l| l.prefix.chars().count() + l.name.chars().count())
        .max()
        .unwrap_or(0);
    let pr_width = all().map(|l| l.pr.len()).max().unwrap_or(0);
    for (header, lines) in &sections {
        if let Some(h) = header {
            out.push((None, format!("{} {}", style.yellow("⊕"), h)));
        }
        for l in lines {
            let pad = width - l.prefix.chars().count() - l.name.chars().count();
            let marker = if l.current {
                style.green("◉")
            } else {
                "○".to_string()
            };
            let name = if l.current {
                style.bold(&l.name)
            } else {
                l.name.clone()
            };
            let mut row = format!("{}{marker} {name}{}", style.dim(&l.prefix), " ".repeat(pad));
            if pr_width > 0 {
                row.push_str(&format!(
                    "  {}{}",
                    style.cyan(&l.pr),
                    " ".repeat(pr_width - l.pr.len())
                ));
            }
            for (warn, note) in &l.notes {
                row.push_str("  ");
                row.push_str(&if *warn {
                    style.yellow(note)
                } else {
                    style.dim(note)
                });
            }
            out.push((Some(l.name.clone()), row.trim_end().to_string()));
        }
    }
    out
}

/// Children drawn under a node: its stack successor, then spurs based on it.
/// Parked stacks are drawn separately, under their sum.
fn display_children(st: &State, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some((si, pos)) = st.locate(name)
        && let Some(next) = st.stacks[si].changes.get(pos + 1)
    {
        out.push(next.clone());
    }
    for s in &st.stacks {
        if matches!(&s.base, Base::Change(b) if b == name) {
            out.push(s.changes[0].clone());
        }
    }
    out
}

fn walk(
    st: &State,
    name: &str,
    indent: &str,
    last: bool,
    current: Option<&str>,
    out: &mut Vec<Line>,
) {
    let branch = if last { "└── " } else { "├── " };
    out.push(line(st, name, format!("{indent}{branch}"), current));
    let child_indent = format!("{indent}{}", if last { "    " } else { "│   " });
    let children = display_children(st, name);
    for (i, c) in children.iter().enumerate() {
        walk(st, c, &child_indent, i + 1 == children.len(), current, out);
    }
}

fn line(st: &State, name: &str, prefix: String, current: Option<&str>) -> Line {
    let change = &st.changes[name];
    let mut notes = Vec::new();
    let parent = st.parent(name);
    let implied_by_parent = match &parent {
        Some(Base::Trunk) => change.deps.is_empty(),
        Some(Base::Change(p)) => change.deps.len() == 1 && change.deps.contains(p),
        Some(Base::Sum(ms)) => change.deps.iter().eq(ms.iter()),
        None => true,
    };
    if !implied_by_parent && !change.deps.is_empty() {
        let deps: Vec<&str> = change.deps.iter().map(String::as_str).collect();
        notes.push((false, format!("← {}", deps.join(", "))));
    }
    if let Some(p) = st.ordering_only_parent(name) {
        notes.push((true, format!("⚠ ordered after {p} (independent)")));
    }
    Line {
        prefix,
        name: name.to_string(),
        current: current == Some(name),
        pr: change.pr.map(|n| format!("#{n}")).unwrap_or_default(),
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{FanIn, add_change};

    #[test]
    fn renders_stacks_spurs_and_parked() {
        let mut st = State::new("main", "origin");
        let add = |st: &mut State, n: &str, d: &[&str], f: FanIn| {
            let d: Vec<String> = d.iter().map(|s| s.to_string()).collect();
            add_change(st, n, &d, f).unwrap();
        };
        add(&mut st, "auth", &[], FanIn::Linearize);
        add(&mut st, "theme", &[], FanIn::Linearize);
        add(&mut st, "login", &["auth", "theme"], FanIn::Linearize);
        add(&mut st, "tests", &["auth"], FanIn::Linearize);
        add(&mut st, "docs", &[], FanIn::Linearize);
        add(&mut st, "search", &["docs", "login"], FanIn::Park);
        st.changes.get_mut("auth").unwrap().pr = Some(12);
        let out = render(&st, Some("login"), &Style::new(false));
        let expected = "\
main
├── ○ auth           #12
│   ├── ○ theme           ⚠ ordered after auth (independent)
│   │   └── ◉ login       ← auth, theme
│   └── ○ tests
└── ○ docs
⊕ parked on docs ⊕ login
└── ○ search
";
        assert_eq!(out, expected, "\n{out}");
    }
}
