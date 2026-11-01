//! tui-v13 panels and controls: `/agents` (D131) with ask status,
//! finalize and stop, the agent strip's actions, `/decisions` (D128) and
//! `/subagents` (D133).
use super::{App, DialogPurpose, Overlay};
use crate::agentsview::{AgentRow, AgentsAction, AgentsView};
use crate::decisionsview::{DecisionsAction, DecisionsView};
use crate::dialog::{Body, Dialog, DialogOption};
use crate::engine::{AgentDone, Review, agent_filters};
use crate::transcript::Level;
use crate::wrap::truncate;
use serde_json::Value;

/// A dialog opened from a panel; the panel comes back after it.
pub(super) enum PanelDialog {
    AgentAsk(String),
    AgentStop(String),
    AgentFinalize(String),
    DecisionChange(String),
    DecisionDiscuss(String),
    Subagents,
}

const STATUS_QUESTION: &str = "What is your current status? What is done, what are you doing now, what is left, and are you blocked on anything?";

impl App {
    // ---- /agents --------------------------------------------------------

    pub(super) fn open_agents(&mut self, select: Option<&str>) {
        let (flat, _) = self.engine.subagents();
        let mut v = AgentsView::new(flat);
        self.fill_agents(&mut v);
        if let Some(id) = select {
            v.select(id);
        }
        self.overlay = Overlay::Agents(Box::new(v));
    }

    fn agent_row(&self, v: &Value) -> AgentRow {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let id = s("agentId");
        let label = self.tree.label(&id);
        let last = v["lastAction"]
            .as_str()
            .or(v["lastTool"].as_str())
            .unwrap_or_default()
            .to_string();
        let cwd = self.engine.cwd().to_path_buf();
        let name = |x: &str| {
            let p = std::path::Path::new(x);
            if p == cwd {
                "the whole project".to_string()
            } else {
                crate::transcript::rel_path(p)
            }
        };
        let paths = match &v["scope"] {
            Value::Object(o) => o.get("paths").and_then(Value::as_array).cloned(),
            Value::Array(p) => Some(p.clone()),
            _ => None,
        };
        let scope = paths
            .map(|p| {
                p.iter()
                    .filter_map(Value::as_str)
                    .map(name)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let cost = match (v["costUsd"].as_f64(), v["unknownCostCalls"].as_u64()) {
            (Some(c), Some(u)) => crate::engine::fmt_cost(c, u64::from(c > 0.0), u),
            (Some(c), None) => format!("${c:.2}"),
            _ => "—".into(),
        };
        AgentRow {
            label: if id == "root" { "root".into() } else { label },
            title: s("title"),
            parent: v["parentId"].as_str().map(str::to_string),
            state: s("state"),
            model: self.model_label(v["model"].as_str()),
            elapsed: v["elapsed"].as_u64().unwrap_or(0),
            tokens: v["tokens"].as_u64().unwrap_or(0),
            cost,
            last,
            descendants: v["descendants"].as_u64().unwrap_or(0),
            scope,
            context: s("context"),
            last_text: crate::markdown::plain(&s("lastText")),
            finalizing: self
                .finalizing
                .iter()
                .any(|f| f.agents.contains(&id) && !f.done.contains(&id)),
            depth: 0,
            order: if id == "root" {
                0
            } else {
                self.tree.index(&id).map_or(usize::MAX, |i| i + 1)
            },
            id,
        }
    }

    fn fill_agents(&self, v: &mut AgentsView) {
        let all = self
            .engine
            .agent_rows(agent_filters(None, None, None, None, ""));
        let mut models: Vec<String> = all
            .as_ref()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r["model"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        models.sort();
        models.dedup();
        v.models = models;
        let (state, context, subtree, model, query) = v.filters();
        let rows = self
            .engine
            .agent_rows(agent_filters(state, context, subtree, model, query))
            .unwrap_or_default();
        let rows = rows.iter().map(|r| self.agent_row(r)).collect();
        v.set_rows(rows);
    }

    /// Re-read open panels (live rows, ledger changes).
    pub(super) fn refresh_panels(&mut self) {
        let mut overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match &mut overlay {
            Overlay::Agents(v) => self.fill_agents(v),
            Overlay::Decisions(d) => d.set(self.engine.decisions(), self.decision_labels()),
            _ => {}
        }
        if matches!(self.overlay, Overlay::None) {
            self.overlay = overlay;
        }
        self.decision_count = self.engine.decision_open_count();
    }

    pub(super) fn agents_action(&mut self, action: AgentsAction) {
        match action {
            AgentsAction::None => {}
            AgentsAction::Close => self.overlay = Overlay::None,
            AgentsAction::Refresh => self.refresh_panels(),
            AgentsAction::Open(id) => {
                self.overlay = Overlay::None;
                self.open_view(&id);
            }
            AgentsAction::Ask(id) => self.ask_dialog(&id),
            AgentsAction::Finalize(id) => self.finalize_dialog(&id),
            AgentsAction::Stop(id) => self.stop_dialog(&id),
        }
    }

    /// Open a panel dialog; the current overlay comes back after it.
    fn panel_dialog(&mut self, dialog: Dialog, p: PanelDialog) {
        let prev = std::mem::replace(
            &mut self.overlay,
            Overlay::Dialog(Box::new(dialog), DialogPurpose::Panel(p)),
        );
        if !matches!(prev, Overlay::None) {
            self.overlay_back = Some(prev);
        }
    }

    pub(super) fn restore_back(&mut self) {
        if matches!(self.overlay, Overlay::None)
            && let Some(o) = self.overlay_back.take()
        {
            self.overlay = o;
            self.refresh_panels();
        }
    }

    pub(super) fn ask_dialog(&mut self, id: &str) {
        if id == "root" {
            return self.hint("ask status works on subagents");
        }
        let label = self.tree.label(id);
        let dialog = Dialog::new(
            format!("Ask {label} for its status"),
            Body::Markdown(format!(
                "A **read-only fork** of {label}'s context answers; {label} itself is not interrupted or told. The answer shows here and as a row in root's transcript."
            )),
            vec![
                DialogOption::new("ask", "Ask").key('a').describe("send the question below"),
                DialogOption::new("cancel", "Cancel"),
            ],
        )
        .with_text("question")
        .prefill(STATUS_QUESTION)
        .text_option("ask", true);
        self.panel_dialog(dialog, PanelDialog::AgentAsk(id.to_string()));
    }

    pub(super) fn stop_dialog(&mut self, id: &str) {
        if id == "root" {
            return self.hint("esc cancels root's turn");
        }
        let label = self.tree.label(id);
        let below = self
            .tree
            .agents
            .iter()
            .filter(|a| a.view.id != id && self.in_subtree(&a.view.id, id))
            .count();
        let dialog = Dialog::new(
            format!("Stop {label}'s branch"),
            Body::Markdown(format!(
                "Stops **{label}**{} now: running calls are cancelled. Its parent gets a partial-progress report built from the journal. The reason is recorded in `/decisions`.\n\nTo let it finish its current call and write a proper report instead, use **finalize**.",
                if below > 0 {
                    format!(" and its {below} descendant{}", if below == 1 { "" } else { "s" })
                } else {
                    String::new()
                }
            )),
            vec![
                DialogOption::new("cancel", "Keep it running"),
                DialogOption::new("stop", "Stop the branch")
                    .key('s')
                    .describe("type the reason below (required)"),
            ],
        )
        .with_text("reason (required)")
        .needs_text(&["stop"]);
        // The reason is required: the field has focus, Enter stops.
        let dialog = dialog.text_option("stop", true);
        self.panel_dialog(dialog, PanelDialog::AgentStop(id.to_string()));
    }

    pub(super) fn finalize_dialog(&mut self, id: &str) {
        if self.engine.subagents().0 {
            return self.hint("flat mode has no finalize · stop ends a child");
        }
        let label = self.tree.label(id);
        let dialog = Dialog::new(
            format!("Finalize {label}"),
            Body::Markdown(format!(
                "Wraps up **{label}** and its subtree gracefully, bottom-up: the deepest agents finish their current call, then get one last turn with tools disabled to write their report; each parent goes after all its children reported, up to {label}. Progress shows as a row."
            )),
            vec![
                DialogOption::new("cancel", "Keep it running"),
                DialogOption::new("finalize", "Finalize").key('f'),
            ],
        );
        self.panel_dialog(dialog, PanelDialog::AgentFinalize(id.to_string()));
    }

    /// A finished ask or stop.
    pub(super) fn poll_agent_done(&mut self) -> bool {
        let mut changed = false;
        while let Ok(done) = self.agent_done.1.try_recv() {
            changed = true;
            match done {
                AgentDone::Asked {
                    id,
                    question,
                    result,
                } => {
                    if let Overlay::Agents(v) = &mut self.overlay {
                        v.asking = None;
                        v.answer = Some((id, question, result));
                    } else if let Err(e) = result {
                        self.hint(format!("ask {}: {e}", self.tree.label(&id)));
                    } else {
                        self.hint(format!(
                            "{} answered · the row in root's transcript has it",
                            self.tree.label(&id)
                        ));
                    }
                }
                AgentDone::Stopped { id, result } => match result {
                    Ok(()) => self.hint(format!("stopped {}", self.tree.label(&id))),
                    Err(e) => self.hint(format!("stop {}: {e}", self.tree.label(&id))),
                },
            }
            self.refresh_panels();
        }
        changed
    }

    /// An agent strip action chip (D131): ask status, stop branch, finalize,
    /// or the agent in `/agents`.
    pub(super) fn strip_act(&mut self, id: &str, act: crate::ui::StripAct) {
        use crate::ui::StripAct;
        match act {
            StripAct::Ask => self.ask_dialog(id),
            StripAct::Stop => self.stop_dialog(id),
            StripAct::Finalize => self.finalize_dialog(id),
            StripAct::Agents => self.open_agents(Some(id)),
        }
    }

    // ---- /decisions -----------------------------------------------------

    /// Agent ids (canonical and `role-first8`) with their display labels,
    /// for the deciding agent and ids quoted in a record's text.
    fn decision_labels(&self) -> Vec<(String, String)> {
        let mut v = vec![];
        for a in &self.tree.agents {
            let id = a.view.id.clone();
            let mut it = id.splitn(2, '-');
            let (role, rest) = (it.next().unwrap_or_default(), it.next().unwrap_or_default());
            v.push((id.clone(), a.label.clone()));
            v.push((
                format!("{role}-{}", &rest[..rest.len().min(8)]),
                a.label.clone(),
            ));
        }
        v
    }

    pub(super) fn open_decisions(&mut self) {
        let v = DecisionsView::new(self.engine.decisions(), self.decision_labels());
        self.overlay = Overlay::Decisions(Box::new(v));
    }

    pub(super) fn decisions_action(&mut self, action: DecisionsAction) {
        match action {
            DecisionsAction::None => {}
            DecisionsAction::Close => self.overlay = Overlay::None,
            DecisionsAction::Keep(id) => {
                match self.engine.review_decision(&id, Review::Keep) {
                    Ok(_) => self.hint("kept · the decision is closed"),
                    Err(e) => self.hint(e),
                }
                self.refresh_panels();
            }
            DecisionsAction::Change(id) => {
                let Some(d) = self.engine.decisions().into_iter().find(|d| d.id == id) else {
                    return;
                };
                let who = self.tree.label(&d.agent);
                let dialog = Dialog::new(
                    format!("Change: {}", truncate(&d.question, 60)),
                    Body::Markdown(format!(
                        "**{who}** chose: {}\n\nWrite what to do instead. It is sent to **{who}** as steering{}; the record closes as *changed* and keeps the original choice.",
                        d.choice,
                        if d.agent == "root" {
                            " (queued for root's next turn)"
                        } else {
                            " (a finished agent resumes for it)"
                        }
                    )),
                    vec![
                        DialogOption::new("send", "Send the change").key('s'),
                        DialogOption::new("cancel", "Cancel"),
                    ],
                )
                .with_text("what to do instead")
                .needs_text(&["send"])
                .text_option("send", true);
                self.panel_dialog(dialog, PanelDialog::DecisionChange(id));
            }
            DecisionsAction::Discuss(id) => {
                let Some(d) = self.engine.decisions().into_iter().find(|d| d.id == id) else {
                    return;
                };
                let who = self.tree.label(&d.agent);
                let dialog = Dialog::new(
                    format!("Discuss with {who}"),
                    Body::Markdown(format!(
                        "Opens a private, read-only side chat with **{who}** about this decision. {who} itself is not interrupted. The record stays open as *discussing*."
                    )),
                    vec![
                        DialogOption::new("discuss", "Discuss").key('d'),
                        DialogOption::new("cancel", "Cancel"),
                    ],
                )
                .with_text("your question (optional)")
                .text_option("discuss", true);
                self.panel_dialog(dialog, PanelDialog::DecisionDiscuss(id));
            }
        }
    }

    // ---- /subagents (D133) ----------------------------------------------

    /// The session header names the mode: keep it true after a change.
    fn mode_changed(&mut self, flat: bool) {
        let (from, to) = if flat {
            ("tree subagents", "flat subagents")
        } else {
            ("flat subagents", "tree subagents")
        };
        for e in &mut self.transcript.entries {
            if let crate::transcript::Kind::Notice(_, text) = &mut e.kind
                && text.contains(" subagents · /help for keys")
            {
                *text = text.replace(from, to);
                e.touch();
            }
        }
        self.transcript.notice(
            Level::Info,
            format!(
                "subagents: {} for this session",
                if flat { "flat" } else { "tree" }
            ),
        );
    }

    pub(super) fn subagents_command(&mut self, arg: &str) {
        let (flat, locked) = self.engine.subagents();
        let default_flat = self.engine.subagents_default();
        let name = |f: bool| if f { "flat" } else { "tree" };
        if !arg.is_empty() {
            let want = match arg {
                "flat" | "1" | "1-layer" => true,
                "tree" => false,
                _ => return self.hint("/subagents tree | flat"),
            };
            if locked && want != flat {
                return self.hint(format!(
                    "subagents are {} for this session (fixed at the first message) · /new starts a session where you can choose",
                    name(flat)
                ));
            }
            match self.engine.set_subagents(want) {
                Ok(()) => self.mode_changed(want),
                Err(e) => self.hint(e),
            }
            return;
        }
        let explain = "**Tree** (default): children can start their own children; you get partial reports, growth notices, finalize and `maxDescendants`.\n\n**Flat** (one layer): only root starts children, children cannot start subagents; several `task` calls in one turn run in parallel and the turn waits for all of them. Children's questions come straight to you.";
        if locked {
            let dialog = Dialog::new(
                "Subagents",
                Body::Markdown(format!(
                    "This session uses **{}** subagents. The mode is fixed once the first message is sent: the tools sent to the model are frozen for the session (prompt cache), so it cannot change now. `/new` starts a session where you can choose; the default for new sessions is **{}** (config `subagents`).\n\n{explain}",
                    name(flat),
                    name(default_flat)
                )),
                vec![DialogOption::new("ok", "OK")],
            );
            self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Viewer);
            return;
        }
        let dialog = Dialog::new(
            "Subagents for this session",
            Body::Markdown(format!(
                "Choose before the first message; it is fixed after that. Config default: **{}**.\n\n{explain}",
                name(default_flat)
            )),
            vec![
                DialogOption::new("tree", if flat { "Tree" } else { "Tree · current" })
                    .key('t')
                    .describe("children may start children"),
                DialogOption::new("flat", if flat { "Flat · current" } else { "Flat" })
                    .key('f')
                    .describe("one layer under root"),
            ],
        )
        .select(name(flat));
        self.panel_dialog(dialog, PanelDialog::Subagents);
    }

    pub(super) fn panel_dialog_chosen(&mut self, p: PanelDialog, id: &str, text: Option<String>) {
        match p {
            PanelDialog::AgentAsk(agent) if id == "ask" => {
                let q = text.unwrap_or_else(|| STATUS_QUESTION.into());
                self.engine.ask_agent(&agent, &q, self.agent_done.0.clone());
                if let Some(Overlay::Agents(v)) = &mut self.overlay_back {
                    v.asking = Some(agent.clone());
                    v.select(&agent);
                }
                self.hint(format!("asking {}…", self.tree.label(&agent)));
            }
            PanelDialog::AgentStop(agent) if id == "stop" => {
                let reason = text.unwrap_or_default();
                self.engine
                    .stop_agent(&agent, &reason, self.agent_done.0.clone());
                self.hint(format!("stopping {}…", self.tree.label(&agent)));
            }
            PanelDialog::AgentFinalize(agent) if id == "finalize" => {
                match self.engine.finalize_agent(&agent) {
                    Ok(list) => self.hint(format!(
                        "finalizing {} · {} agent{} wrap up, deepest first",
                        self.tree.label(&agent),
                        list.len(),
                        if list.len() == 1 { "" } else { "s" }
                    )),
                    Err(e) => self.hint(e),
                }
            }
            PanelDialog::DecisionChange(did) if id == "send" => {
                let steering = text.unwrap_or_default();
                match self.engine.review_decision(&did, Review::Change(steering)) {
                    Ok(_) => self.hint("changed · steering sent to the deciding agent"),
                    Err(e) => self.hint(e),
                }
            }
            PanelDialog::DecisionDiscuss(did) if id == "discuss" => {
                match self.engine.review_decision(&did, Review::Discuss(text)) {
                    Ok(Some(fork)) => {
                        let agent = self
                            .engine
                            .decisions()
                            .into_iter()
                            .find(|d| d.id == did)
                            .map(|d| d.agent)
                            .unwrap_or_else(|| "root".into());
                        let mut chat = crate::forkchat::ForkChat::with_summary(
                            self.tree.label(&agent),
                            vec![],
                        );
                        chat.side = true;
                        chat.fork = Some(fork);
                        self.overlay_back = None;
                        self.overlay = Overlay::ForkChat(Box::new(chat));
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => self.hint(e),
                }
            }
            PanelDialog::Subagents => {
                let want = id == "flat";
                match self.engine.set_subagents(want) {
                    Ok(()) => self.mode_changed(want),
                    Err(e) => self.hint(e),
                }
            }
            _ => {}
        }
        self.restore_back();
    }
}
