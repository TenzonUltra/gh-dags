//! Moving around the layout (`up`, `down`, `top`, `bottom`). Stacks are
//! linear, but the layout is a forest: `up` can branch (a stack successor plus
//! spurs) and `down` can too (a parked change sits on a sum). `up` follows the
//! stack when it can and says what it passed; anything else ambiguous stops
//! and lists the choices.

use anyhow::{Result, bail};

use crate::layout::describe_base;
use crate::model::{Base, State};

/// Where a move ends up. `None` is the trunk.
#[derive(Debug, PartialEq, Eq)]
pub struct Nav {
    pub target: Option<String>,
    /// Something worth saying: changes skipped at a fork, or why it stopped.
    pub note: Option<String>,
}

/// Changes sitting directly on `from` (the trunk when `None`): the stack
/// successor first, then spurs, then parked stacks.
fn above(st: &State, from: Option<&str>) -> Vec<String> {
    let Some(from) = from else {
        return st.roots();
    };
    let mut out = Vec::new();
    if let Some((si, pos)) = st.locate(from)
        && let Some(next) = st.stacks[si].changes.get(pos + 1)
    {
        out.push(next.clone());
    }
    for s in &st.stacks {
        if matches!(&s.base, Base::Change(b) if b == from) {
            out.push(s.changes[0].clone());
        }
    }
    for s in &st.stacks {
        if matches!(&s.base, Base::Sum(ms) if ms.iter().any(|m| m == from)) {
            out.push(s.changes[0].clone());
        }
    }
    out
}

fn has_successor(st: &State, name: &str) -> bool {
    !st.is_top(name)
}

fn where_(st: &State, at: Option<&str>) -> String {
    at.unwrap_or(&st.trunk).to_string()
}

pub fn up(st: &State, from: Option<&str>, n: usize) -> Result<Nav> {
    let mut at = from.map(String::from);
    let mut skipped = Vec::new();
    for step in 0..n {
        let options = above(st, at.as_deref());
        let next = match options.as_slice() {
            [] => {
                if step == 0 {
                    bail!(
                        "already at the top: nothing sits on {}",
                        where_(st, at.as_deref())
                    );
                }
                break;
            }
            [only] => only.clone(),
            [first, rest @ ..] if at.as_deref().is_some_and(|a| has_successor(st, a)) => {
                skipped.extend(rest.iter().cloned());
                first.clone()
            }
            many => {
                if step == 0 {
                    bail!(
                        "{} has several changes on it: {}; pick one with `gh dags switch <name>`",
                        where_(st, at.as_deref()),
                        many.join(", ")
                    );
                }
                return Ok(Nav {
                    note: Some(format!(
                        "stopped at {}, which has several changes on it: {}",
                        where_(st, at.as_deref()),
                        many.join(", ")
                    )),
                    target: at,
                });
            }
        };
        at = Some(next);
    }
    Ok(Nav {
        target: at,
        note: (!skipped.is_empty())
            .then(|| format!("also branching off on the way: {}", skipped.join(", "))),
    })
}

pub fn down(st: &State, from: &str, n: usize) -> Result<Nav> {
    let mut at = from.to_string();
    for step in 0..n {
        match st.parent(&at) {
            Some(Base::Change(p)) => at = p,
            Some(Base::Trunk) | None => {
                if step == 0 {
                    bail!(
                        "already at the bottom: {at} sits on {}; `gh dags trunk` switches there",
                        st.trunk
                    );
                }
                break;
            }
            Some(base @ Base::Sum(_)) => {
                let sum = describe_base(st, &base);
                if step == 0 {
                    bail!("{at} is parked on {sum}; pick one with `gh dags switch <name>`");
                }
                return Ok(Nav {
                    target: Some(at.clone()),
                    note: Some(format!("stopped at {at}, which is parked on {sum}")),
                });
            }
        }
    }
    Ok(Nav {
        target: Some(at),
        note: None,
    })
}

/// The top of the current stack (from trunk: of the only stack on it).
pub fn top(st: &State, from: Option<&str>) -> Result<Nav> {
    let start = match from {
        Some(c) => c.to_string(),
        None => only_root(st)?,
    };
    let (si, _) = st.locate(&start).unwrap();
    Ok(Nav {
        target: st.stacks[si].changes.last().cloned(),
        note: None,
    })
}

/// The bottom of the current stack (from trunk: of the only stack on it).
pub fn bottom(st: &State, from: Option<&str>) -> Result<Nav> {
    let start = match from {
        Some(c) => c.to_string(),
        None => only_root(st)?,
    };
    let (si, _) = st.locate(&start).unwrap();
    Ok(Nav {
        target: st.stacks[si].changes.first().cloned(),
        note: None,
    })
}

fn only_root(st: &State) -> Result<String> {
    match st.roots().as_slice() {
        [] => bail!("there are no changes yet"),
        [one] => Ok(one.clone()),
        many => bail!(
            "{} has several stacks on it: {}; pick one with `gh dags switch <name>`",
            st.trunk,
            many.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{FanIn, add_change};

    /// main ← auth ← tests, theme spur on auth, theme ← login; docs on main;
    /// search parked on docs ⊕ login.
    fn st() -> State {
        let mut st = State::new("main", "origin");
        for (n, d, f) in [
            ("auth", vec![], FanIn::Linearize),
            ("tests", vec!["auth"], FanIn::Linearize),
            ("theme", vec!["auth"], FanIn::Linearize),
            ("login", vec!["theme"], FanIn::Linearize),
            ("docs", vec![], FanIn::Linearize),
            ("search", vec!["docs", "login"], FanIn::Park),
        ] {
            let d: Vec<String> = d.into_iter().map(String::from).collect();
            add_change(&mut st, n, &d, f).unwrap();
        }
        st
    }

    fn to(name: &str) -> Option<String> {
        Some(name.to_string())
    }

    #[test]
    fn up_follows_the_stack_and_mentions_spurs() {
        let st = st();
        let nav = up(&st, Some("auth"), 1).unwrap();
        assert_eq!(nav.target, to("tests"));
        assert_eq!(
            nav.note.as_deref(),
            Some("also branching off on the way: theme")
        );
        assert_eq!(up(&st, Some("theme"), 5).unwrap().target, to("search"));
        assert!(
            up(&st, Some("tests"), 1)
                .unwrap_err()
                .to_string()
                .contains("already at the top")
        );
    }

    #[test]
    fn up_from_trunk_needs_a_single_stack() {
        let st = st();
        let err = up(&st, None, 1).unwrap_err().to_string();
        assert!(
            err.contains("main has several changes on it: auth, docs"),
            "{err}"
        );
    }

    #[test]
    fn down_crosses_from_a_spur_into_its_anchor() {
        let st = st();
        assert_eq!(down(&st, "login", 2).unwrap().target, to("auth"));
        assert_eq!(
            down(&st, "login", 9).unwrap().target,
            to("auth"),
            "clamped at the bottom"
        );
        assert!(
            down(&st, "auth", 1)
                .unwrap_err()
                .to_string()
                .contains("already at the bottom")
        );
        let err = down(&st, "search", 1).unwrap_err().to_string();
        assert!(err.contains("parked on docs ⊕ login"), "{err}");
    }

    #[test]
    fn top_and_bottom_stay_in_the_current_stack() {
        let st = st();
        assert_eq!(top(&st, Some("auth")).unwrap().target, to("tests"));
        assert_eq!(top(&st, Some("theme")).unwrap().target, to("login"));
        assert_eq!(bottom(&st, Some("login")).unwrap().target, to("theme"));
        assert!(bottom(&st, None).is_err(), "two stacks sit on trunk");
    }
}
