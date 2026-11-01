//! tui-v13 transcript rows for the agent tree: D132 partial reports,
//! subtree growth, finalize progress, D131 stop and status-ask outcomes,
//! the D125 compaction suggestion, and the D134 wake header.
use super::App;
use crate::agents::Tree;
use crate::engine::TreeRow;
use crate::transcript::{Card, CardPart, Tone, Transcript};
use crate::wrap::truncate;

/// Live wrap-up progress of one `agents finalize` request (D132).
pub(super) struct Finalizing {
    /// Who asked (its transcript holds the row).
    pub caller: String,
    pub target: String,
    /// Deepest first, as the engine orders them.
    pub agents: Vec<String>,
    pub wrapping: Vec<String>,
    pub done: Vec<String>,
}

fn first_line(text: &str, n: usize) -> String {
    truncate(
        text.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .trim(),
        n,
    )
}

/// `title [id] model=m startedBy=s depth=d` from a growth notice.
fn growth_parts(line: &str) -> (String, String, String, String, String) {
    let (title, rest) = line.rsplit_once(" [").unwrap_or((line, ""));
    let (id, rest) = rest.split_once("] ").unwrap_or((rest, ""));
    let field = |k: &str| {
        rest.split_whitespace()
            .find_map(|w| w.strip_prefix(k))
            .unwrap_or_default()
            .to_string()
    };
    (
        title.to_string(),
        id.trim_end_matches(']').to_string(),
        field("model="),
        field("startedBy="),
        field("depth="),
    )
}

fn short_model(m: &str) -> String {
    m.rsplit('/').next().unwrap_or(m).to_string()
}

/// The card for a tree row as it appears in its own journal's transcript
/// (replay, and live for rows with no aggregate state).
pub(super) fn tree_card(tree: &Tree, row: &TreeRow) -> Card {
    match row {
        TreeRow::Partial { child, text } => Card {
            key: format!("partial:{child}"),
            icon: "◐".into(),
            tone: Tone::Info,
            head: format!(
                "partial report · {} · {}",
                tree.label(child),
                first_line(text, 120)
            ),
            body: vec![CardPart::Md(format!(
                "_Latest partial report from **{}** (replaces the previous one; the final report follows when it finishes)._\n\n{text}",
                tree.label(child)
            ))],
        },
        TreeRow::Growth(lines) => {
            let parts: Vec<_> = lines.iter().map(|l| growth_parts(l)).collect();
            let names = parts
                .iter()
                .map(|(t, id, m, _, _)| {
                    let who = if t.is_empty() {
                        tree.label(id)
                    } else {
                        t.clone()
                    };
                    if m.is_empty() || m == "inherited" {
                        who
                    } else {
                        format!("{who} ({})", short_model(m))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let mut md =
                String::from("| agent | model | started by | depth |\n|---|---|---|---|\n");
            for (t, id, m, by, d) in &parts {
                md.push_str(&format!(
                    "| {} `{}` | {} | {} | {d} |\n",
                    if t.is_empty() { "—" } else { t },
                    tree.label(id),
                    if m.is_empty() { "inherited" } else { m },
                    tree.label(by),
                ));
            }
            Card {
                key: String::new(),
                icon: "↳".into(),
                tone: Tone::Info,
                head: format!(
                    "{} new descendant{} · {names}",
                    parts.len(),
                    if parts.len() == 1 { "" } else { "s" }
                ),
                body: vec![CardPart::Md(md)],
            }
        }
        TreeRow::FinalizeRequested { target, agents } => finalize_card(
            tree,
            &Finalizing {
                caller: String::new(),
                target: target.clone(),
                agents: agents.clone(),
                wrapping: vec![],
                done: vec![],
            },
            false,
        ),
        TreeRow::FinalTurn => Card {
            key: "final-turn".into(),
            icon: "◇".into(),
            tone: Tone::Warn,
            head: "final turn · tools disabled · writing the report for its parent".into(),
            body: vec![],
        },
        TreeRow::Finalized => Card {
            key: "final-turn".into(),
            icon: "✓".into(),
            tone: Tone::Ok,
            head: "finalized · final report sent to its parent".into(),
            body: vec![],
        },
        TreeRow::Stopped {
            target,
            reason,
            report,
        } => Card {
            key: format!("stop:{target}"),
            icon: "■".into(),
            tone: Tone::Warn,
            head: format!(
                "stopped {} and its subtree · {}",
                tree.label(target),
                first_line(reason, 100)
            ),
            body: vec![CardPart::Md(format!(
                "**Reason:** {reason}\n\n**Partial progress sent to its parent:**\n\n{report}"
            ))],
        },
        TreeRow::Asked {
            target,
            question,
            answer,
            error,
        } => {
            let (icon, tone, tail) = match (answer, error) {
                (Some(a), _) => ("?", Tone::Info, first_line(a, 120)),
                (None, Some(e)) => ("✗", Tone::Err, format!("failed: {}", first_line(e, 100))),
                (None, None) => ("?", Tone::Info, "asking…".into()),
            };
            let mut md = format!(
                "**Asked {}:** {question}\n\n_Answered by a read-only fork of its context; {} was not interrupted or told._\n\n",
                tree.label(target),
                tree.label(target)
            );
            if let Some(a) = answer {
                md.push_str(a);
            } else if let Some(e) = error {
                md.push_str(&format!("**Error:** {e}"));
            }
            Card {
                key: format!("ask:{target}:{}", question.len()),
                icon: icon.into(),
                tone,
                head: format!("status of {} · {tail}", tree.label(target)),
                body: vec![CardPart::Md(md)],
            }
        }
    }
}

/// The finalize progress row: who is wrapping up, bottom-up.
pub(super) fn finalize_card(tree: &Tree, f: &Finalizing, live: bool) -> Card {
    let n = f.agents.len();
    let done = f.done.len();
    let wrapping: Vec<String> = f
        .wrapping
        .iter()
        .filter(|a| !f.done.contains(a))
        .map(|a| tree.label(a))
        .collect();
    let head = if !live {
        format!(
            "finalize {} · {n} agent{} wrap up, deepest first",
            tree.label(&f.target),
            if n == 1 { "" } else { "s" }
        )
    } else if done >= n && n == 1 {
        format!(
            "finalized {} · its final report is in",
            tree.label(&f.target)
        )
    } else if done >= n && n > 0 {
        format!("finalized {} · all {n} reported", tree.label(&f.target))
    } else if wrapping.is_empty() {
        format!(
            "finalizing {} · {done}/{n} reported · waiting for in-flight calls",
            tree.label(&f.target)
        )
    } else {
        format!(
            "finalizing {} · {done}/{n} reported · wrapping up: {}",
            tree.label(&f.target),
            wrapping.join(", ")
        )
    };
    let mut md = String::from(
        "Each agent finishes its current call, then gets one last turn with tools disabled to write its report; parents go after all their children reported.\n\n",
    );
    for a in &f.agents {
        let state = if f.done.contains(a) {
            "✓ reported"
        } else if f.wrapping.contains(a) {
            "◇ final turn"
        } else if live {
            "◌ waiting"
        } else {
            "·"
        };
        md.push_str(&format!("- {state} · **{}**\n", tree.label(a)));
    }
    Card {
        key: format!("finalize:{}", f.target),
        icon: if live && done >= n && n > 0 {
            "✓"
        } else {
            "◇"
        }
        .into(),
        tone: if live && done >= n && n > 0 {
            Tone::Ok
        } else {
            Tone::Warn
        },
        head,
        body: vec![CardPart::Md(md)],
    }
}

/// D125: the root compaction suggestion, once per crossing.
pub(super) fn compact_card(pct: u64, used: u64, window: u64) -> Card {
    use crate::picker::fmt_tokens;
    Card {
        key: String::new(),
        icon: "◔".into(),
        tone: Tone::Warn,
        head: format!(
            "root context {pct}% · {}/{} · consider /compact at a stopping point",
            fmt_tokens(used),
            fmt_tokens(window)
        ),
        body: vec![CardPart::Md(
            "The root never compacts itself. Run `/compact` (optionally with a focus) when the current step is done; the model was told it may ask you to. This shows once each time the context rises past the mark (`rootCompactSuggestAt`, default 75%).".into(),
        )],
    }
}

impl App {
    /// A live tree row (D131/D132) committed in `agent`'s journal.
    pub(super) fn on_tree(&mut self, agent: String, row: TreeRow) {
        match &row {
            TreeRow::Partial { child, .. } => {
                // It goes to the parent, like the report it previews.
                let parent = self
                    .tree
                    .get(child)
                    .map(|a| a.view.parent.clone())
                    .unwrap_or_else(|| "root".into());
                if parent == "root" && !self.running {
                    self.wake_reasons
                        .push(format!("partial report from {}", self.tree.label(child)));
                }
                let card = tree_card(&self.tree, &row);
                if let Some(tr) = self.transcript_of(&parent) {
                    tr.card(card);
                }
            }
            TreeRow::FinalizeRequested { target, agents } => {
                let f = Finalizing {
                    caller: agent.clone(),
                    target: target.clone(),
                    agents: agents.clone(),
                    wrapping: vec![],
                    done: vec![],
                };
                let card = finalize_card(&self.tree, &f, true);
                self.finalizing.retain(|x| x.target != *target);
                self.finalizing.push(f);
                if let Some(tr) = self.transcript_of(&agent) {
                    tr.card(card);
                }
            }
            TreeRow::FinalTurn | TreeRow::Finalized => {
                let finished = matches!(row, TreeRow::Finalized);
                let card = tree_card(&self.tree, &row);
                if let Some(tr) = self.transcript_of(&agent) {
                    tr.card(card);
                }
                let mut updates = vec![];
                for f in &mut self.finalizing {
                    if !f.agents.contains(&agent) {
                        continue;
                    }
                    if !f.wrapping.contains(&agent) {
                        f.wrapping.push(agent.clone());
                    }
                    if finished && !f.done.contains(&agent) {
                        f.done.push(agent.clone());
                    }
                    updates.push((f.caller.clone(), finalize_card(&self.tree, f, true)));
                }
                for (caller, card) in updates {
                    if let Some(tr) = self.transcript_of(&caller) {
                        tr.card(card);
                    }
                }
            }
            TreeRow::Growth(lines) => {
                let Some(row) = own_growth(&self.tree, &agent, lines) else {
                    return;
                };
                let card = tree_card(&self.tree, &row);
                if let Some(tr) = self.transcript_of(&agent) {
                    tr.card(card);
                }
            }
            _ => {
                let card = tree_card(&self.tree, &row);
                if let Some(tr) = self.transcript_of(&agent) {
                    tr.card(card);
                }
            }
        }
        self.refresh_panels();
    }

    /// D134: root started a turn by itself. When mail woke it (children
    /// reported, a message, a scope request), say so above the turn.
    pub(super) fn wake_header(&mut self) {
        let reasons = std::mem::take(&mut self.wake_reasons);
        if reasons.is_empty() {
            return;
        }
        let mut uniq: Vec<String> = vec![];
        for r in reasons {
            if !uniq.contains(&r) {
                uniq.push(r);
            }
        }
        let head = format!("children reported · {}", uniq.join(" · "));
        let body = format!(
            "Root was idle, so this mail started a turn by itself:\n\n{}\n\nEsc cancels it; after a cancel, mail waits until you send a message.",
            uniq.iter()
                .map(|r| format!("- {r}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        self.transcript.card(Card {
            key: String::new(),
            icon: "↻".into(),
            tone: Tone::Accent,
            head,
            body: vec![CardPart::Md(body)],
        });
    }
}

/// A growth notice without the children `agent` started itself (its own
/// `task` rows show those); None when nothing is left.
fn own_growth(tree: &Tree, agent: &str, lines: &[String]) -> Option<TreeRow> {
    let short = |id: &str| {
        let mut it = id.splitn(2, '-');
        let role = it.next().unwrap_or_default();
        let rest = it.next().unwrap_or_default();
        format!("{role}-{}", &rest[..rest.len().min(8)])
    };
    let me = [agent.to_string(), short(agent), tree.label(agent)];
    let kept: Vec<String> = lines
        .iter()
        .filter(|l| {
            let (_, _, _, by, _) = growth_parts(l);
            !me.contains(&by)
        })
        .cloned()
        .collect();
    (!kept.is_empty()).then_some(TreeRow::Growth(kept))
}

/// Replay: a tree row into a transcript being filled (`agent`'s journal).
pub(super) fn fill_tree(t: &mut Transcript, tree: &Tree, agent: &str, row: &TreeRow) {
    if let TreeRow::Growth(lines) = row {
        if let Some(row) = own_growth(tree, agent, lines) {
            t.card(tree_card(tree, &row));
        }
        return;
    }
    t.card(tree_card(tree, row));
}
