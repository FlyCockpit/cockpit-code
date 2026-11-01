//! engine-o rows and dialogs: D120 scope requests and subscription notices,
//! D121 guidance (rows and `/guidance`), `/prune`, and the D122 `!cmd`
//! notice row.
use super::{App, DialogPurpose, Overlay, Prompt};
use crate::agents::Tree;
use crate::dialog::{Body, Dialog, DialogOption};
use crate::engine::{ChangeView, GuidanceSel, LocalRun, PruneView, ScopeChoice, ScopeView};
use crate::picker::fmt_tokens;
use crate::transcript::{Card, CardPart, Level, Tone};
use crate::wrap::truncate;
use std::path::Path;

fn rel(p: &Path) -> String {
    crate::transcript::rel_path(p)
}

/// `who → owner`, with display labels.
fn route(tree: &Tree, v: &ScopeView) -> String {
    format!("{} → {}", tree.label(&v.requester), tree.label(&v.owner))
}

/// What a scope request's status means, in a few words.
fn scope_status(v: &ScopeView) -> (&'static str, Tone, String) {
    match v.status.as_str() {
        "pending" if v.escalation.is_some() => {
            ("⇄", Tone::Warn, "waiting for you · /answer".into())
        }
        "pending" => ("⇄", Tone::Warn, "waiting for its owner".into()),
        "applying" => ("⇄", Tone::Info, "applying".into()),
        "applied" => ("✓", Tone::Ok, "applied".into()),
        "granted" => ("✓", Tone::Ok, "scope granted · requester retries".into()),
        "rejected" => ("✗", Tone::Err, "rejected".into()),
        "stale" => ("!", Tone::Warn, "stale · back to the requester".into()),
        "unavailable" => ("!", Tone::Warn, "owner unavailable".into()),
        "failed" => ("✗", Tone::Err, "failed".into()),
        other => ("·", Tone::Info, other.to_string()),
    }
}

/// The proposed change of a scope request as a diff part.
fn scope_diff(v: &ScopeView) -> Option<CardPart> {
    let path = rel(&v.path);
    let s = |k: &str| v.args[k].as_str().unwrap_or_default().to_string();
    match v.tool.as_str() {
        "write" => Some(CardPart::Diff {
            path,
            old: String::new(),
            new: s("content"),
            write: true,
        }),
        "edit" => {
            // One replacement, or an `edits` list of them.
            let (mut old, mut new) = (s("old_string"), s("new_string"));
            if let Some(list) = v.args["edits"].as_array() {
                let pick = |k: &str| {
                    list.iter()
                        .filter_map(|e| e[k].as_str())
                        .collect::<Vec<_>>()
                        .join("\n…\n")
                };
                old = pick("old_string");
                new = pick("new_string");
            }
            Some(CardPart::Diff {
                path,
                old,
                new,
                write: false,
            })
        }
        _ => None,
    }
}

pub(super) fn scope_card(tree: &Tree, v: &ScopeView) -> Card {
    let (icon, tone, status) = scope_status(v);
    let head = format!(
        "scope request · {} · {} {} · {status}",
        route(tree, v),
        v.tool,
        rel(&v.path)
    );
    let mut md = format!(
        "**{}** asked to {} `{}`, outside its write scope. Owner: **{}**{}.",
        tree.label(&v.requester),
        v.tool,
        rel(&v.path),
        tree.label(&v.owner),
        if v.wait {
            " · the requester is waiting"
        } else {
            " · the requester keeps working"
        }
    );
    if let Some(r) = v.reason.as_deref().filter(|r| !r.trim().is_empty()) {
        md.push_str(&format!("\n\nreason: {}", r.trim()));
    }
    if let Some(o) = v.output.as_deref().filter(|o| !o.trim().is_empty()) {
        md.push_str(&format!("\n\nresult: {}", truncate(o.trim(), 600)));
    }
    md.push_str(&format!("\n\nrequest `{}`", &v.id[..v.id.len().min(8)]));
    let mut body = vec![CardPart::Md(md)];
    body.extend(scope_diff(v));
    Card {
        key: format!("scope:{}", v.id),
        icon: icon.into(),
        tone,
        head,
        body,
    }
}

/// Batched changes to subscribed paths (D120): who changed what, and the
/// bounded diffs on expand.
pub(super) fn subscription_card(tree: &Tree, changes: &[ChangeView], overflow: bool) -> Card {
    let who = |c: &ChangeView| match &c.source {
        Some(a) => tree.label(a),
        None => "external".into(),
    };
    let by = |c: &ChangeView| match &c.source {
        Some(a) => format!("changed by {}", tree.label(a)),
        None => "changed outside the agents (an editor or another program)".into(),
    };
    let list = changes
        .iter()
        .map(|c| format!("{} ({})", rel(&c.path), who(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let n = changes.len();
    let head = format!(
        "{n} watched path{} changed · {list}{}",
        if n == 1 { "" } else { "s" },
        if overflow { " · more coalesced" } else { "" }
    );
    let mut body = vec![];
    for c in changes {
        let text = c.text.trim();
        let fence = if text.lines().any(|l| l.starts_with("@@")) {
            "diff"
        } else {
            "text"
        };
        body.push(CardPart::Md(format!(
            "**{}** · {}\n\n```{fence}\n{text}\n```",
            rel(&c.path),
            by(c)
        )));
    }
    if overflow {
        body.push(CardPart::Md(
            "_More changes were coalesced; the agent was told to re-read its watched paths._"
                .into(),
        ));
    }
    Card {
        key: String::new(),
        icon: "✎".into(),
        tone: Tone::Info,
        head,
        body,
    }
}

fn trigger_text(s: &GuidanceSel) -> String {
    format!(
        "{} write to `{}` · {} · area `{}`",
        if s.first_write {
            "first"
        } else {
            "new language/area"
        },
        rel(&s.path),
        s.language,
        s.area
    )
}

/// A guidance injection (D121) as a distinct, collapsible row.
pub(super) fn guidance_card(s: &GuidanceSel) -> Card {
    let (tone, head) = if let Some(e) = &s.error {
        (Tone::Warn, format!("guidance failed · {}", truncate(e, 80)))
    } else if s.items.is_empty() {
        (
            Tone::Info,
            format!("guidance · nothing pertinent for {}", rel(&s.path)),
        )
    } else {
        let items = s
            .items
            .iter()
            .map(|(id, title, _)| format!("{id} {title}"))
            .collect::<Vec<_>>()
            .join(", ");
        (Tone::Accent, format!("guidance · {items}"))
    };
    let mut md = format!("after the {}", trigger_text(s));
    if !s.reason.trim().is_empty() {
        md.push_str(&format!("\n\nwhy: {}", s.reason.trim()));
    }
    for (id, title, text) in &s.items {
        md.push_str(&format!("\n\n- **{id} {title}.** {}", text.trim()));
    }
    md.push_str(&format!(
        "\n\n_selected by {} · context {}_",
        s.model, s.context
    ));
    Card {
        key: format!("guidance:{}", s.id),
        icon: "✦".into(),
        tone,
        head,
        body: vec![CardPart::Md(md)],
    }
}

pub(super) fn prune_card(r: &PruneView) -> Card {
    if r.saved == 0 {
        return Card {
            key: String::new(),
            icon: "✂".into(),
            tone: Tone::Info,
            head: "nothing to prune · the prompt cache is unchanged".into(),
            body: vec![],
        };
    }
    Card {
        key: String::new(),
        icon: "✂".into(),
        tone: Tone::Ok,
        head: format!(
            "pruned · saved ~{} tokens ({} → {}) · {}{}",
            fmt_tokens(r.saved),
            fmt_tokens(r.before),
            fmt_tokens(r.after),
            prune_counts(r),
            if r.cache_break {
                " · cache restarts"
            } else {
                ""
            }
        ),
        body: vec![CardPart::Md(
            "Old delegation prompts and bulky tool outputs were replaced with stubs in the model's view. The full originals stay on disk (the `history` tool reads them)."
                .into(),
        )],
    }
}

fn prune_counts(r: &PruneView) -> String {
    let n = |k: usize, one: &str, many: &str| {
        (k > 0).then(|| format!("{k} {}", if k == 1 { one } else { many }))
    };
    let parts: Vec<String> = [
        n(r.delegations, "delegation prompt", "delegation prompts"),
        n(r.tool_outputs, "tool output", "tool outputs"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "old payloads stubbed".into()
    } else {
        format!("{} stubbed", parts.join(" and "))
    }
}

use crate::transcript::fmt_bytes;

/// A `!cmd` run as the agent was told about it (replay: the output was
/// never kept).
pub(super) fn local_run_card(r: &LocalRun) -> Card {
    Card {
        key: String::new(),
        icon: "!".into(),
        tone: Tone::Info,
        head: format!(
            "{} · exit {} · {} · {} output · not kept, not sent to the model",
            truncate(r.command.lines().next().unwrap_or_default(), 60),
            r.exit_code.map_or("?".into(), |c| c.to_string()),
            if r.duration_ms < 1000 {
                format!("{}ms", r.duration_ms)
            } else {
                format!("{:.1}s", r.duration_ms as f64 / 1000.0)
            },
            fmt_bytes(r.output_bytes)
        ),
        body: vec![],
    }
}

impl App {
    /// A scope request changed: upsert its row for the requester and the
    /// owner, and keep root's owner dialog in step.
    pub(super) fn on_scope(&mut self, v: ScopeView) {
        let card = scope_card(&self.tree, &v);
        for who in [v.requester.clone(), v.owner.clone()] {
            if let Some(tr) = self.transcript_of(&who) {
                tr.card(card.clone());
            }
            if v.requester == v.owner {
                break;
            }
        }
        let known = self
            .prompts
            .iter()
            .position(|p| matches!(p, Prompt::Scope(x, _) if x.id == v.id));
        if v.escalation.is_some() && v.status == "pending" {
            match known {
                Some(i) => {
                    if let Prompt::Scope(x, _) = &mut self.prompts[i] {
                        *x = v;
                    }
                }
                None => self.prompts.push_back(Prompt::Scope(v, false)),
            }
        } else if known.is_some() {
            self.remove_prompt(&v.id);
            if matches!(&self.overlay, Overlay::Dialog(_, DialogPurpose::Scope(id)) if *id == v.id)
            {
                self.overlay = Overlay::None;
            }
        }
    }

    /// The owner dialog for a scope request root owns (D120).
    pub(super) fn scope_dialog(&mut self, v: &ScopeView) {
        let path = rel(&v.path);
        // D124: the owner's reason comes first; it is why this is here.
        let mut body = vec![];
        if let Some(why) = &v.escalation {
            body.push(Body::Markdown(format!(
                "**{} escalated this to you:** {}\n\n",
                self.tree.label(&v.owner),
                why.trim()
            )));
        }
        body.push(Body::Markdown(format!(
            "**{}** wants to **{}** `{path}`, outside its write scope{}.\n\n",
            self.tree.label(&v.requester),
            v.tool,
            if v.wait {
                "; it is **waiting** for your answer"
            } else {
                "; it keeps working meanwhile"
            }
        )));
        if let Some(CardPart::Diff {
            path,
            old,
            new,
            write,
        }) = scope_diff(v)
        {
            body.push(Body::FileDiff {
                path,
                old,
                new,
                start: write.then_some((1, 1)),
            });
        }
        body.push(Body::Markdown(
            "_Apply re-checks the file and still goes through your permission mode._".into(),
        ));
        let dir = v
            .path
            .parent()
            .map(rel)
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| ".".into());
        let dialog = Dialog::new(
            format!("Write outside scope · {}", route(&self.tree, v)),
            Body::Parts(body),
            vec![
                DialogOption::new("apply", "Apply")
                    .key('a')
                    .describe("make this change for the requester"),
                DialogOption::new("reject", "Reject with a reason")
                    .key('r')
                    .describe("type the reason below; the requester reads it"),
                DialogOption::new("grant", "Grant scope")
                    .key('g')
                    .describe(format!(
                        "let it write there itself: `{dir}`, or directories typed below (comma-separated)"
                    )),
            ],
        )
        .with_text("reason (reject) or directories (grant)")
        .needs_text(&["reject"]);
        self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Scope(v.id.clone()));
    }

    pub(super) fn scope_chosen(&mut self, rid: &str, id: &str, text: Option<String>) {
        let Some(v) = self.prompts.iter().find_map(|p| match p {
            Prompt::Scope(x, _) if x.id == rid => Some(x.clone()),
            _ => None,
        }) else {
            return;
        };
        let choice = match id {
            "apply" => ScopeChoice::Apply,
            "reject" => ScopeChoice::Reject(text.unwrap_or_default()),
            "grant" => {
                let cwd = self.engine.cwd().to_path_buf();
                let dirs: Vec<std::path::PathBuf> = match text {
                    Some(t) if !t.trim().is_empty() => t
                        .split(',')
                        .map(str::trim)
                        .filter(|d| !d.is_empty())
                        .map(|d| cwd.join(d))
                        .collect(),
                    _ => v
                        .path
                        .parent()
                        .map(|p| p.to_path_buf())
                        .into_iter()
                        .collect(),
                };
                ScopeChoice::Grant(dirs)
            }
            _ => return,
        };
        self.remove_prompt(rid);
        self.engine
            .resolve_scope(rid, choice, self.async_errors.0.clone());
    }

    /// Errors from background engine calls (a failed scope answer).
    pub(super) fn poll_async_errors(&mut self) -> bool {
        let mut any = false;
        while let Ok(e) = self.async_errors.1.try_recv() {
            self.transcript.notice(Level::Error, e);
            any = true;
        }
        any
    }

    pub(super) fn on_subscription(
        &mut self,
        agent: &str,
        changes: Vec<ChangeView>,
        overflow: bool,
    ) {
        let card = subscription_card(&self.tree, &changes, overflow);
        if let Some(tr) = self.transcript_of(agent) {
            tr.card(card);
        }
    }

    pub(super) fn on_guidance(&mut self, s: GuidanceSel) {
        let card = guidance_card(&s);
        if let Some(tr) = self.transcript_of(&s.agent) {
            tr.card(card);
        }
    }

    /// `/guidance`: what was injected, per agent, and why (D121).
    pub(super) fn open_guidance(&mut self) {
        let (enabled, model, max) = self.engine.guidance_config();
        let all = self.engine.guidance_selections(None);
        let mut md = String::new();
        let state = if enabled {
            format!(
                "**on** · selector model: {} · up to {max} items per injection",
                model.as_deref().unwrap_or("the writing agent's own model")
            )
        } else {
            "**off**".to_string()
        };
        md.push_str(&format!("Contextual guidance is {state}.\n\n"));
        if all.is_empty() {
            if enabled {
                md.push_str("Nothing selected yet. After an agent's first successful `write` or `edit` of a turn, and again when its writes move into a new language or area, a selector picks the pertinent items from your guidance library and appends them to that tool result.");
            } else {
                md.push_str("To turn it on, add `\"guidance\": {\"enabled\": true}` to `config.json` (optional: `\"model\": \"provider/model\"` for the selector, `\"maxItems\": 1-5`); it applies from the next turn.\n\nThe library is `guidance/*.md` in the config directory (`cockpit-code init` writes a starter set) plus `.cockpit-code/guidance/*.md` in the project once you trust it. Items look like `- **RS3 Title.** text`.");
            }
        } else {
            let mut agents: Vec<String> = vec![];
            for s in &all {
                if !agents.contains(&s.agent) {
                    agents.push(s.agent.clone());
                }
            }
            for a in agents {
                let mine: Vec<&GuidanceSel> = all.iter().filter(|s| s.agent == a).collect();
                let items: usize = mine.iter().map(|s| s.items.len()).sum();
                md.push_str(&format!(
                    "### {} · {} selection{}, {items} item{}\n\n",
                    self.tree.label(&a),
                    mine.len(),
                    if mine.len() == 1 { "" } else { "s" },
                    if items == 1 { "" } else { "s" }
                ));
                for s in mine {
                    let when = chrono::DateTime::parse_from_rfc3339(&s.created)
                        .map(|t| crate::transcript::fmt_stamp(&t.with_timezone(&chrono::Local)))
                        .unwrap_or_default();
                    md.push_str(&format!(
                        "**{when}** · {} · context {} · {}\n\n",
                        trigger_text(s),
                        s.context,
                        s.model
                    ));
                    if let Some(e) = &s.error {
                        md.push_str(&format!("- failed: {e}\n"));
                    } else if s.items.is_empty() {
                        md.push_str("- nothing pertinent\n");
                    }
                    for (id, title, text) in &s.items {
                        md.push_str(&format!("- **{id} {title}.** {}\n", text.trim()));
                    }
                    if !s.reason.trim().is_empty() {
                        md.push_str(&format!("\n_why: {}_\n", s.reason.trim()));
                    }
                    md.push('\n');
                }
            }
        }
        let dialog = Dialog::new(
            "Guidance",
            Body::Markdown(md),
            vec![DialogOption::new("close", "Close").key('q')],
        );
        self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Viewer);
    }

    /// `/prune`: preview, then a confirm that says it breaks the cache.
    pub(super) fn prune_command(&mut self) {
        if self.running || self.compacting {
            return self.hint("wait for the turn to finish, then /prune");
        }
        let r = match self.engine.prune_preview() {
            Ok(r) => r,
            Err(e) => return self.transcript.notice(Level::Error, format!("prune: {e}")),
        };
        if r.saved == 0 {
            let card = prune_card(&r);
            self.transcript.card(card);
            return;
        }
        let pct = r.saved as f64 / r.before.max(1) as f64 * 100.0;
        let md = format!(
            "| | tokens |\n|---|---|\n| now | {} |\n| after | {} |\n| saved | **{}** ({pct:.0}%) |\n\n{}; the latest {} assistant/tool batches stay as they are. User messages, criteria and every call/result id are kept, and the originals stay on disk.\n\n**This breaks the prompt cache:** the next request re-sends the whole pruned context at the uncached price once. Worth it when the savings are large or the cache has expired anyway.",
            fmt_tokens(r.before),
            fmt_tokens(r.after),
            fmt_tokens(r.saved),
            {
                let c = prune_counts(&r);
                let mut chars = c.chars();
                chars
                    .next()
                    .map(|f| f.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            },
            r.tail_batches,
        );
        let dialog = Dialog::new(
            "Prune old context?",
            Body::Markdown(md),
            vec![
                DialogOption::new("prune", "Prune")
                    .key('p')
                    .describe("stub them now; no model call"),
                DialogOption::new("cancel", "Cancel").key('c'),
            ],
        )
        .select("cancel");
        self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Prune);
    }

    pub(super) fn prune_confirmed(&mut self) {
        if self.running || self.compacting {
            return self.hint("a turn started; /prune again when it ends");
        }
        self.engine.prune();
        self.hint("pruning…");
    }

    /// D122: a finished `!cmd` tells the viewed agent what ran (not the output).
    pub(super) fn post_local_run(&mut self, agent: &str, run: LocalRun) {
        if self.read_only.is_some() {
            return;
        }
        if let Err(e) = self.engine.post_local_run(agent, &run) {
            self.transcript.notice(
                Level::Warn,
                format!("couldn't tell {agent} about the command: {e}"),
            );
        }
    }
}
