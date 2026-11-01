//! Advisory hints only; command execution, exit status and loop blocking are unchanged.
use crate::permissions::classify::{Classification, classify};
use std::collections::{BTreeSet, VecDeque};
#[derive(Clone, Default)]
pub(super) struct History {
    pub context: u32,
    recent: VecDeque<Observation>,
    shown: BTreeSet<Tip>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tip {
    Read,
    Search,
    Discover,
    BsdSed,
}
impl Tip {
    const ALL: [Self; 4] = [Self::Read, Self::Search, Self::Discover, Self::BsdSed];
    fn text(self) -> &'static str {
        match self {
            Self::Read => {
                "Use read for line-numbered, byte-paged file inspection and reliable continuation offsets."
            }
            Self::Search => {
                "Use grep for bounded file searches with path/line citations and continuation offsets."
            }
            Self::Discover => "Use glob for ignore-aware file discovery.",
            Self::BsdSed => {
                "macOS uses BSD sed: -i requires a backup suffix; use sed -i '' ... for no backup, or edit for an inspectable replacement."
            }
        }
    }
}
impl History {
    /// Restore only emitted static tips from the current durable context. Dynamic
    /// loop/probe observations deliberately start fresh after a process restart.
    pub fn restore(context: u32, was_shown: impl Fn(&str) -> bool) -> Self {
        Self {
            context,
            recent: VecDeque::new(),
            shown: Tip::ALL
                .into_iter()
                .filter(|tip| was_shown(tip.text()))
                .collect(),
        }
    }
}
#[derive(Clone)]
struct Observation {
    segments: Vec<String>,
    target: Option<String>,
    flags: BTreeSet<String>,
    empty_success: bool,
}
pub(super) fn append(
    history: &mut History,
    command: &str,
    success: bool,
    stdout: &str,
    stderr: &str,
) -> String {
    let Classification::Parsed {
        simple_commands, ..
    } = classify(command)
    else {
        return String::new();
    };
    let mut hints = vec![];
    for c in &simple_commands {
        let program = c.normalized_program.rsplit('/').next().unwrap_or("");
        let operands = c.args.iter().filter(|a| !a.starts_with('-')).count();
        match program {
            "cat" | "head" if operands > 0 => Some(Tip::Read),
            "grep" | "rg" if operands > 1 => Some(Tip::Search),
            "find"
                if !c
                    .args
                    .iter()
                    .any(|a| matches!(a.as_str(), "-exec" | "-execdir" | "-delete")) =>
            {
                Some(Tip::Discover)
            }
            "sed"
                if cfg!(target_os = "macos")
                    && c.args.iter().enumerate().any(|(i, a)| {
                        a == "-i"
                            && c.args.get(i + 1).is_none_or(|s| {
                                !s.trim_matches(['\'', '"']).is_empty() && !s.starts_with('.')
                            })
                    }) =>
            {
                Some(Tip::BsdSed)
            }
            _ => None,
        }
        .into_iter()
        .for_each(|tip| {
            if history.shown.insert(tip) {
                hints.push(tip.text());
            }
        });
    }
    let segments = simple_commands
        .iter()
        .map(|c| format!("{} {}", c.normalized_program, c.args.join(" ")))
        .collect::<Vec<_>>();
    let args = simple_commands
        .iter()
        .flat_map(|c| &c.args)
        .collect::<Vec<_>>();
    let target = args
        .iter()
        .rev()
        .find(|a| !a.starts_with('-') && (a.contains('/') || a.contains('.')))
        .map(|a| a.trim_matches(['\'', '"']).to_owned());
    let flags = args
        .iter()
        .filter(|a| a.starts_with('-'))
        .map(|a| (*a).clone())
        .collect();
    let current = Observation {
        segments,
        target,
        flags,
        empty_success: success && stdout.trim().is_empty() && stderr.trim().is_empty(),
    };
    let recent = &mut history.recent;
    if recent.len() >= 2 {
        let chain = [
            &recent[recent.len() - 2],
            &recent[recent.len() - 1],
            &current,
        ];
        let refining = chain.windows(2).all(|w| {
            let (prev, next) = (&w[0].segments, &w[1].segments);
            next.len() == prev.len() + 1
                && next[..prev.len()] == prev[..]
                && next.last().is_some_and(|s| {
                    (s.starts_with("grep ") || s.starts_with("rg "))
                        && s.split_whitespace()
                            .any(|s| s.starts_with('-') && s.contains('v'))
                        || s.starts_with("awk ") && s.contains("!/")
                        || s.starts_with("sed ") && s.contains("/d")
                })
        });
        if refining {
            hints.push("Filter-refinement loop: three successive commands keep narrowing the same source. Read it directly and choose one targeted query instead of adding more exclusion filters.");
        }
    }
    if current.empty_success
        && current.target.is_some()
        && recent
            .iter()
            .rev()
            .take(4)
            .filter(|p| p.empty_success && p.target == current.target && p.flags != current.flags)
            .count()
            >= 2
    {
        hints.push("Repeated empty exit-0 probes: empty is a successful result for this target. Confirm the path or choose a different inspection; changing flags again is unlikely to help.");
    }
    recent.push_back(current);
    while recent.len() > 8 {
        recent.pop_front();
    }
    hints.sort_unstable();
    hints.dedup();
    hints.into_iter().map(|h| format!("\n[tip: {h}]")).collect()
}
