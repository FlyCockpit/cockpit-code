//! `/goal` and `/criteria` (D88–D97, D110): the draft approval, proposal
//! and roadblock dialogs, the goal status dialog, the contract editor and
//! the actions the engine accepts only between turns.
use super::{App, DialogPurpose, EditTarget, EditorReq, Overlay, Prompt};
use crate::criteria::{ContractEditor, CriteriaAction, CriteriaView, EditorAction};
use crate::dialog::{Body, Dialog, DialogOption};
use crate::engine::{ContractDraft, ContractView, GoalView, ProposalView, RoadblockView, Section};
use crate::transcript::{Kind, Level};
use crate::wrap::truncate;
use ratatui::style::{Modifier, Style};

/// A goal dialog waiting in the prompt queue (with approvals and questions).
#[derive(Clone)]
pub(super) enum GoalPrompt {
    Draft(ContractView),
    Proposal {
        agent: String,
        proposal: ProposalView,
    },
    Roadblock {
        agent: String,
        roadblock: RoadblockView,
    },
}

impl GoalPrompt {
    pub(super) fn id(&self) -> String {
        match self {
            GoalPrompt::Draft(_) => "goal-draft".into(),
            GoalPrompt::Proposal { proposal, .. } => proposal.id.clone(),
            GoalPrompt::Roadblock { roadblock, .. } => roadblock.id.clone(),
        }
    }
    pub(super) fn agent(&self) -> &str {
        match self {
            GoalPrompt::Draft(_) => "root",
            GoalPrompt::Proposal { agent, .. } | GoalPrompt::Roadblock { agent, .. } => agent,
        }
    }
}

/// What the contract editor is for.
#[derive(Clone)]
pub(super) enum EditPurpose {
    /// The `/goal` draft, approved on save.
    Approve,
    /// `/goal edit` of a paused or roadblocked goal.
    Edit,
    /// A roadblock pivot with contract edits (the roadblock id).
    Pivot(String),
}

/// An engine action that needs the root to be idle (approve, resume,
/// roadblock resolution); deferred to the end of the running turn.
pub(super) enum GoalAction {
    Approve(Option<ContractDraft>),
    Resume,
    Roadblock {
        id: String,
        resolution: &'static str,
        text: Option<String>,
        draft: Option<ContractDraft>,
    },
}

/// A contract as markdown for dialog bodies.
fn contract_markdown(c: &ContractView, sections: &[Section]) -> String {
    let mut md = String::new();
    if !c.objective.trim().is_empty() {
        md.push_str(&format!("**Objective** {}\n\n", c.objective.trim()));
    }
    for &s in sections {
        let list = c.list(s);
        if list.is_empty() {
            continue;
        }
        md.push_str(&format!("**{}**\n\n", s.title()));
        for item in list {
            let (mark, _) = crate::criteria::mark(item);
            md.push_str(&format!("- {mark}{}", item.text.trim()));
            let meta = crate::criteria::meta(item, s == Section::Steps);
            if item.status != "open" && !meta.is_empty() {
                md.push_str(&format!(" _({meta})_"));
            }
            if let Some(k) = &item.check {
                md.push_str(&format!(" · check `{}`", k.trim()));
            }
            md.push('\n');
        }
        md.push('\n');
    }
    md
}

fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s / 60) % 60)
    }
}

impl App {
    /// Re-read the goal and the root contract; live overlays follow.
    pub(super) fn refresh_goal(&mut self) {
        self.goal = self.engine.goal().filter(|g| g.live());
        self.contract = self.engine.contract("root");
        if let Overlay::Criteria(v) = &mut self.overlay {
            let contract = if v.agent == "root" {
                self.contract.clone()
            } else {
                self.engine.contract(&v.agent)
            };
            v.set_contract(contract);
            if v.agent == "root" {
                v.goal_line = self.goal.as_ref().map(|g| (goal_line(g), goal_style(g)));
                v.editable = self.goal.as_ref().is_some_and(|g| g.editable());
            }
        }
    }

    /// A transcript line when the goal's state changed since the last one.
    pub(super) fn announce_goal(&mut self) {
        let Some(g) = self.goal.clone() else {
            self.goal_announced = None;
            return;
        };
        let state = g.state_text();
        if self.goal_announced.as_deref() == Some(state.as_str()) {
            return;
        }
        self.goal_announced = Some(state);
        let level = match g.disposition.as_str() {
            "roadblocked" => Level::Error,
            "paused" if !g.awaiting_approval() => Level::Warn,
            _ => Level::Info,
        };
        let mut text = goal_line(&g);
        if let Some(r) = g.pause_reason.as_deref().filter(|r| !r.is_empty())
            && g.disposition == "paused"
            && !g.awaiting_approval()
        {
            text.push_str(&format!(" — {}", pause_text(r)));
        }
        self.transcript.notice(level, text);
    }

    /// `/goal [text | pause | resume | edit | clear | status]`.
    pub(super) fn goal_command(&mut self, arg: &str) {
        match arg {
            "" | "status" => self.open_goal_status(),
            "pause" => match &self.goal {
                Some(g) if g.disposition == "running" => {
                    // The pause cancels the running action: not an error.
                    if self.running {
                        self.user_cancelled = true;
                    }
                    match self.engine.goal_pause() {
                        Ok(()) => self.hint("goal paused · /goal resume continues"),
                        Err(e) => self.transcript.notice(Level::Error, e),
                    }
                }
                Some(g) => self.hint(format!("the goal is {} already", g.state_text())),
                None => self.hint("no goal · /goal <text> starts one"),
            },
            "resume" => match &self.goal {
                Some(g) if g.awaiting_approval() => self.reopen_goal_prompts(),
                Some(g) if g.disposition == "paused" => self.run_goal_action(GoalAction::Resume),
                Some(g) if g.disposition == "roadblocked" => {
                    self.hint("the goal is roadblocked: resolve it (pivot, stop or override)");
                    self.reopen_goal_prompts();
                }
                Some(g) => self.hint(format!("the goal is {}", g.state_text())),
                None => self.hint("no goal · /goal <text> starts one"),
            },
            "edit" => self.open_contract_editor(EditPurpose::Edit),
            "clear" => {
                if self.goal.is_none() {
                    return self.hint("no goal to clear");
                }
                let dialog = Dialog::new(
                    "Clear the goal?",
                    Body::Markdown(
                        "This stops the goal and empties root's criteria. The history stays in the transcript."
                            .into(),
                    ),
                    vec![
                        DialogOption::new("clear", "Clear goal").key('y'),
                        DialogOption::new("keep", "Keep").key('n'),
                    ],
                )
                .select("keep");
                self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::GoalClear);
            }
            text => self.goal_start(text),
        }
    }

    fn goal_start(&mut self, text: &str) {
        if self.read_only.is_some() {
            return self.hint("read-only: /fork, /new or /resume to continue");
        }
        if let Some(g) = &self.goal
            && g.disposition != "complete"
        {
            return self.hint(format!(
                "one goal per session: this one is {} · /goal clear first",
                g.state_text()
            ));
        }
        if self.running {
            return self.hint("wait for the turn to end (or esc), then /goal");
        }
        match self.engine.goal_start(text) {
            Ok(()) => {
                self.set_title(self.engine.title());
                self.transcript.push(Kind::User(format!("/goal {text}")));
                self.transcript.begin_attempt();
                self.start();
                self.phase = super::Phase::Waiting;
                self.refresh_goal();
                self.hint("drafting a contract (read-only) · you approve it before work starts");
            }
            Err(e) => self.transcript.notice(Level::Error, e),
        }
    }

    /// Run `a` now, or when the running turn has ended.
    pub(super) fn run_goal_action(&mut self, a: GoalAction) {
        if self.running || self.engine.is_active() {
            self.deferred_goal = Some(a);
            self.hint("applies when the current turn ends");
            return;
        }
        let started = match a {
            GoalAction::Approve(draft) => self.engine.goal_approve(draft.as_ref()).map(|()| {
                self.prompts
                    .retain(|p| !matches!(p, Prompt::Goal(GoalPrompt::Draft(_), _)));
                self.transcript
                    .notice(Level::Info, "goal approved · working until the criteria are met");
                true
            }),
            GoalAction::Resume => self.engine.goal_resume().map(|()| {
                self.transcript.notice(Level::Info, "goal resumed");
                true
            }),
            GoalAction::Roadblock {
                id,
                resolution,
                text,
                draft,
            } => self
                .engine
                .resolve_roadblock(&id, resolution, text.clone(), draft.as_ref())
                .inspect(|_| {
                    self.remove_prompt(&id);
                    let note = text
                        .filter(|t| !t.trim().is_empty())
                        .map(|t| format!(": {}", truncate(t.trim(), 80)))
                        .unwrap_or_default();
                    self.transcript.notice(
                        Level::Info,
                        match resolution {
                            "stop" => "roadblock: stopped · the goal stays roadblocked (/goal edit, /goal clear)".to_string(),
                            r => format!("roadblock: {r}{note}"),
                        },
                    );
                }),
        };
        match started {
            Ok(true) => {
                self.transcript.begin_attempt();
                self.start();
                self.phase = super::Phase::Waiting;
            }
            Ok(false) => {}
            Err(e) => self.transcript.notice(Level::Error, e),
        }
        self.refresh_goal();
        // The action's own line says what happened.
        self.goal_announced = self.goal.as_ref().map(|g| g.state_text());
        self.scroll = None;
    }

    /// A deferred goal action, once root is idle.
    pub(super) fn poll_goal_action(&mut self) -> bool {
        if self.deferred_goal.is_some() && !self.running && !self.engine.is_active() {
            let a = self.deferred_goal.take().unwrap();
            self.run_goal_action(a);
            return true;
        }
        false
    }

    /// Put-off goal dialogs come back.
    fn reopen_goal_prompts(&mut self) {
        let mut any = false;
        for p in &mut self.prompts {
            if let Prompt::Goal(_, deferred) = p {
                *deferred = false;
                any = true;
            }
        }
        if any && matches!(self.overlay, Overlay::None) {
            self.next_prompt();
        }
    }

    /// The dialog for a waiting goal prompt (from `next_prompt`).
    pub(super) fn goal_prompt_dialog(&mut self, p: &GoalPrompt) {
        match p {
            GoalPrompt::Draft(c) => {
                let mut md = contract_markdown(c, &Section::ALL);
                md.push_str("_Approving makes these criteria yours; the agent can only propose changes to them._");
                let dialog = Dialog::new(
                    "Approve the goal contract?",
                    Body::Markdown(md),
                    vec![
                        DialogOption::new("approve", "Approve and start")
                            .key('a')
                            .describe("root works autonomously until every acceptance or findings criterion is met and the review passes"),
                        DialogOption::new("edit", "Edit…")
                            .key('e')
                            .describe("change, add or remove items and checks"),
                        DialogOption::new("editor", "Edit in $EDITOR")
                            .key('g')
                            .describe("the contract as YAML"),
                        DialogOption::new("cancel", "Cancel the goal")
                            .key('c')
                            .describe("discard the draft"),
                    ],
                );
                self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::GoalDraft);
            }
            GoalPrompt::Proposal { agent, proposal } => {
                let label = self.tree.label(agent);
                let contract = if agent == "root" {
                    self.contract.clone()
                } else {
                    self.engine.contract(agent)
                };
                let current = contract.criterion(&proposal.criterion_id);
                let now = current.map_or("(criterion not found)".to_string(), |c| c.text.clone());
                let check = |c: Option<&str>| match c.map(str::trim).filter(|c| !c.is_empty()) {
                    Some(c) => format!("`{c}`"),
                    None => "_none_".to_string(),
                };
                let old_check = current.and_then(|c| c.check.as_deref());
                let new_check = proposal.check.as_deref();
                // engine-o: the proposal replaces the check too; no check removes it.
                let checks = if old_check.map(str::trim) == new_check.map(str::trim) {
                    format!("**Check** {} _(unchanged)_", check(new_check))
                } else if new_check.is_none() {
                    format!("**Check** {} → _removed_", check(old_check))
                } else {
                    format!("**Check** {} → {}", check(old_check), check(new_check))
                };
                let md = format!(
                    "**{label}** proposes changing a criterion you own.\n\n**Now** {now}\n\n**Proposed** {}\n\n{checks}\n\n**Why** {}\n\n_Accepting or editing reopens the criterion and clears its evidence; the proposed check replaces the current one. Your edit changes the text only (the contract editor changes checks)._",
                    proposal.change.trim(),
                    proposal.reason.trim()
                );
                let dialog = Dialog::new(
                    format!("Criteria proposal · {label}"),
                    Body::Markdown(md),
                    vec![
                        DialogOption::new("accept", "Accept").key('a'),
                        DialogOption::new("edit", "Accept with my edit")
                            .key('e')
                            .describe("edit the text below, then enter"),
                        DialogOption::new("reject", "Reject")
                            .key('r')
                            .describe("the criterion stays as it is"),
                    ],
                )
                .with_text("criterion text")
                .prefill(proposal.change.trim())
                .text_option("edit", false);
                self.overlay = Overlay::Dialog(
                    Box::new(dialog),
                    DialogPurpose::Proposal(agent.clone(), proposal.id.clone()),
                );
            }
            GoalPrompt::Roadblock { agent, roadblock } => {
                let label = self.tree.label(agent);
                let mut md = format!("**{label}** stopped at a roadblock");
                if let Some(c) = roadblock
                    .criterion_id
                    .as_deref()
                    .and_then(|id| self.contract.criterion(id))
                {
                    md.push_str(&format!(" on “{}”", c.text.trim()));
                }
                md.push_str(&format!(".\n\n{}\n\n", roadblock.reason.trim()));
                if !roadblock.options.is_empty() {
                    md.push_str("**Options it suggests**\n\n");
                    for o in &roadblock.options {
                        md.push_str(&format!("- {}\n", o.trim()));
                    }
                    md.push('\n');
                }
                md.push_str("_Only root's goal waits; the rest of the tree keeps working._");
                let dialog = Dialog::new(
                    format!("Roadblock · {label}"),
                    Body::Markdown(md),
                    vec![
                        DialogOption::new("pivot", "Pivot with a message")
                            .key('p')
                            .describe("type instructions below; the goal continues with them"),
                        DialogOption::new("pivot-edit", "Pivot: edit the contract…")
                            .key('e')
                            .describe("change the criteria, then continue"),
                        DialogOption::new("override", "Override")
                            .key('o')
                            .describe("continue, recording the note below as an accepted risk"),
                        DialogOption::new("stop", "Stop").key('s').describe(
                            "the goal stays roadblocked; /goal edit or /goal clear later",
                        ),
                    ],
                )
                .with_text("message or override note")
                .needs_text(&["pivot", "override"]);
                self.overlay = Overlay::Dialog(
                    Box::new(dialog),
                    DialogPurpose::Roadblock(roadblock.id.clone()),
                );
            }
        }
    }

    /// A goal dialog put off with Esc: `/goal` or `/answer` reopens it.
    pub(super) fn goal_dialog_later(&mut self, id: &str) {
        for p in &mut self.prompts {
            if let Prompt::Goal(g, deferred) = p
                && g.id() == id
            {
                *deferred = true;
            }
        }
        self.hint("put off · /goal or /answer reopens it");
    }

    pub(super) fn goal_dialog_chosen(
        &mut self,
        purpose: DialogPurpose,
        choice: &str,
        text: Option<String>,
    ) {
        match purpose {
            DialogPurpose::GoalDraft => match choice {
                "approve" => self.run_goal_action(GoalAction::Approve(None)),
                "edit" => self.open_contract_editor(EditPurpose::Approve),
                "editor" => {
                    let draft = ContractDraft::from_view(&self.draft_contract());
                    self.contract_external(EditPurpose::Approve, draft);
                }
                _ => match self.engine.goal_cancel_draft() {
                    Ok(()) => {
                        self.prompts
                            .retain(|p| !matches!(p, Prompt::Goal(GoalPrompt::Draft(_), _)));
                        self.transcript.notice(Level::Info, "goal cancelled");
                        self.refresh_goal();
                    }
                    Err(e) => self.transcript.notice(Level::Error, e),
                },
            },
            DialogPurpose::Proposal(agent, id) => {
                let (resolution, text) = match choice {
                    "edit" => match text.filter(|t| !t.trim().is_empty()) {
                        Some(t) => ("edit", Some(t.trim().to_string())),
                        None => ("accept", None),
                    },
                    "accept" => ("accept", None),
                    _ => ("reject", None),
                };
                self.remove_prompt(&id);
                match self.engine.resolve_proposal(&agent, &id, resolution, text) {
                    Ok(()) => {
                        let label = self.tree.label(&agent);
                        self.transcript.notice(
                            Level::Info,
                            format!("criteria proposal from {label}: {resolution}ed"),
                        );
                        self.refresh_goal();
                    }
                    Err(e) => self.transcript.notice(Level::Error, e),
                }
            }
            DialogPurpose::Roadblock(id) => match choice {
                "pivot-edit" => self.open_contract_editor(EditPurpose::Pivot(id)),
                "pivot" | "override" | "stop" => {
                    let resolution = match choice {
                        "pivot" => "pivot",
                        "override" => "override",
                        _ => "stop",
                    };
                    let text = if resolution == "stop" { None } else { text };
                    self.run_goal_action(GoalAction::Roadblock {
                        id,
                        resolution,
                        text,
                        draft: None,
                    });
                }
                _ => {}
            },
            DialogPurpose::GoalStatus => match choice {
                "pause" => self.goal_command("pause"),
                "resume" => self.goal_command("resume"),
                "edit" => self.goal_command("edit"),
                "clear" => self.goal_command("clear"),
                "criteria" => self.open_criteria(),
                "review" => self.reopen_goal_prompts(),
                _ => {}
            },
            DialogPurpose::GoalClear if choice == "clear" => match self.engine.goal_clear() {
                Ok(()) => {
                    self.prompts.retain(|p| !matches!(p, Prompt::Goal(..)));
                    self.deferred_goal = None;
                    self.transcript.notice(Level::Info, "goal cleared");
                    self.refresh_goal();
                }
                Err(e) => self.transcript.notice(Level::Error, e),
            },
            _ => {}
        }
    }

    /// The draft waiting for approval (from its prompt, else the engine).
    fn draft_contract(&self) -> ContractView {
        self.prompts
            .iter()
            .find_map(|p| match p {
                Prompt::Goal(GoalPrompt::Draft(c), _) => Some(c.clone()),
                _ => None,
            })
            .unwrap_or_else(|| self.engine.contract("root"))
    }

    pub(super) fn open_contract_editor(&mut self, purpose: EditPurpose) {
        let (title, verb, base) = match &purpose {
            EditPurpose::Approve => ("Edit the goal contract", "approve", self.draft_contract()),
            EditPurpose::Edit => {
                match &self.goal {
                    None => return self.hint("no goal · /goal <text> starts one"),
                    Some(g) if !g.editable() => {
                        return self.hint("pause the goal first (/goal pause), then /goal edit");
                    }
                    Some(g) if g.awaiting_approval() => {
                        return self.open_contract_editor(EditPurpose::Approve);
                    }
                    _ => {}
                }
                (
                    "Edit the goal contract",
                    "save",
                    self.engine.contract("root"),
                )
            }
            EditPurpose::Pivot(_) => (
                "Pivot: edit the contract",
                "pivot",
                self.engine.contract("root"),
            ),
        };
        let draft = ContractDraft::from_view(&base);
        self.show_contract_editor(purpose, title, verb, draft, None);
    }

    fn show_contract_editor(
        &mut self,
        purpose: EditPurpose,
        title: &str,
        verb: &'static str,
        draft: ContractDraft,
        message: Option<String>,
    ) {
        let mut ed = ContractEditor::new(title, verb, draft);
        if matches!(purpose, EditPurpose::Pivot(_)) {
            ed = ed.without_steps();
        }
        ed.message = message;
        self.overlay = Overlay::Contract(Box::new(ed), purpose);
    }

    pub(super) fn contract_editor_action(&mut self, action: EditorAction) {
        if matches!(action, EditorAction::None) {
            return;
        }
        let Overlay::Contract(ed, purpose) = std::mem::replace(&mut self.overlay, Overlay::None)
        else {
            return;
        };
        match action {
            EditorAction::None => {}
            EditorAction::Cancel => {
                // A cancelled draft or pivot edit returns to its dialog (next_prompt).
            }
            EditorAction::External => self.contract_external(purpose, ed.draft),
            EditorAction::Save => self.save_contract(purpose, ed.draft),
        }
    }

    fn save_contract(&mut self, purpose: EditPurpose, draft: ContractDraft) {
        match purpose {
            EditPurpose::Approve => self.run_goal_action(GoalAction::Approve(Some(draft))),
            EditPurpose::Edit => match self.engine.goal_edit(&draft) {
                Ok(()) => {
                    self.transcript.notice(
                        Level::Info,
                        "goal contract saved · changed items reopened · /goal resume continues",
                    );
                    self.refresh_goal();
                }
                Err(e) => {
                    let msg = e.clone();
                    self.show_contract_editor(
                        EditPurpose::Edit,
                        "Edit the goal contract",
                        "save",
                        draft,
                        Some(msg),
                    );
                }
            },
            EditPurpose::Pivot(id) => self.run_goal_action(GoalAction::Roadblock {
                id,
                resolution: "pivot",
                text: Some("Contract edited by the user.".into()),
                draft: Some(draft),
            }),
        }
    }

    /// Edit the contract as YAML in `$EDITOR`; the form reopens with the result.
    fn contract_external(&mut self, purpose: EditPurpose, draft: ContractDraft) {
        self.editor = Some(EditorReq {
            text: draft.to_yaml(),
            target: EditTarget::Contract(purpose, Box::new(draft)),
        });
    }

    /// The `$EDITOR` result for a contract: back to the form to review.
    pub(super) fn contract_edited(
        &mut self,
        purpose: EditPurpose,
        old: ContractDraft,
        result: Result<Option<String>, String>,
    ) {
        let (title, verb) = match &purpose {
            EditPurpose::Approve => ("Edit the goal contract", "approve"),
            EditPurpose::Edit => ("Edit the goal contract", "save"),
            EditPurpose::Pivot(_) => ("Pivot: edit the contract", "pivot"),
        };
        match result {
            Ok(Some(text)) if text.trim().is_empty() => self.show_contract_editor(
                purpose,
                title,
                verb,
                old,
                Some("empty file: nothing changed".into()),
            ),
            Ok(Some(text)) => match ContractDraft::from_yaml(&text) {
                Ok(d) => self.show_contract_editor(
                    purpose,
                    title,
                    verb,
                    d,
                    Some(format!("from $EDITOR · ctrl+s {verb}")),
                ),
                Err(e) => self.show_contract_editor(purpose, title, verb, old, Some(e)),
            },
            Ok(None) => self.show_contract_editor(
                purpose,
                title,
                verb,
                old,
                Some("editor exited with an error: nothing changed".into()),
            ),
            Err(e) => self.show_contract_editor(purpose, title, verb, old, Some(e)),
        }
    }

    /// `/criteria`: the viewed agent's contract.
    pub(super) fn open_criteria(&mut self) {
        let agent = self.view.clone().unwrap_or_else(|| "root".into());
        let label = self.tree.label(&agent);
        let contract = if agent == "root" {
            self.contract.clone()
        } else {
            self.engine.contract(&agent)
        };
        let mut v = CriteriaView::new(agent.clone(), label, contract);
        if agent == "root" {
            v.goal_line = self.goal.as_ref().map(|g| (goal_line(g), goal_style(g)));
            v.editable = self.goal.as_ref().is_some_and(|g| g.editable());
        }
        self.overlay = Overlay::Criteria(Box::new(v));
    }

    pub(super) fn criteria_action(&mut self, action: CriteriaAction) {
        match action {
            CriteriaAction::None => {}
            CriteriaAction::Close => self.overlay = Overlay::None,
            CriteriaAction::Edit => self.open_contract_editor(EditPurpose::Edit),
            CriteriaAction::Pin => {
                self.show_goals = !self.show_goals;
                self.overlay = Overlay::None;
                self.hint(match (self.show_goals, self.wide) {
                    (true, true) => "criteria panel on (/criteria, p to hide)",
                    (true, false) => "criteria panel on; it shows at 100+ columns",
                    _ => "criteria panel off",
                });
            }
        }
    }

    /// `/goal`: state, checklist, counters, gaps and history.
    pub(super) fn open_goal_status(&mut self) {
        self.refresh_goal();
        let Some(g) = self.goal.clone() else {
            if !self.contract.is_empty() {
                return self.open_criteria();
            }
            return self
                .hint("no goal · /goal <text> drafts a contract, then works until it is met");
        };
        let (met, total, verified) = g.contract.counts();
        let mut md = format!("**{}**", goal_line(&g));
        if let Some(r) = g.pause_reason.as_deref().filter(|r| !r.is_empty())
            && g.disposition == "paused"
        {
            md.push_str(&format!(" — {}", pause_text(r)));
        }
        md.push_str("\n\n");
        md.push_str(&format!(
            "criteria {met}/{total} met ({verified} verified) · turns {} · {} tokens · {}{} · active {}",
            g.turns,
            crate::picker::fmt_tokens(g.tokens),
            crate::engine::fmt_cost(g.cost_usd, u64::from(g.cost_usd > 0.0), g.unknown_cost_calls),
            if g.review_panel {
                format!(" · review {}/{}", g.review_rounds, g.max_review_rounds)
            } else {
                " · review panel off".into()
            },
            fmt_secs(g.active_seconds)
        ));
        let mut limits = vec![];
        if let Some(u) = g.max_usd {
            limits.push(format!("${u:.2}"));
        }
        if let Some(m) = g.max_minutes {
            limits.push(format!("{m:.0} min"));
        }
        if let Some(t) = g.max_tokens {
            limits.push(format!("{} tokens", crate::picker::fmt_tokens(t)));
        }
        md.push_str(&format!(
            "\n\nlimits: {} · no progress {}/{} turns · same gaps {}/{} rounds{} · model `{}`\n\n",
            if limits.is_empty() {
                "none".to_string()
            } else {
                limits.join(" · ")
            },
            g.no_progress_turns,
            g.no_progress_limit,
            g.repeated_gap_rounds,
            g.repeated_gap_limit,
            if g.usage_resumes > 0 {
                format!(" · usage retries {}", g.usage_resumes)
            } else {
                String::new()
            },
            g.model
        ));
        md.push_str(&contract_markdown(&g.contract, &Section::ALL));
        if !g.gaps.is_empty() {
            md.push_str("**Gaps from the last review**\n\n");
            for gap in &g.gaps {
                md.push_str(&format!("- {}\n", gap.trim()));
            }
            md.push('\n');
        }
        if let Some(n) = g.next_step.as_deref().filter(|n| !n.trim().is_empty()) {
            md.push_str(&format!("**Next step** {}\n\n", n.trim()));
        }
        if !g.history.is_empty() {
            md.push_str("**History**\n\n");
            for (at, disposition, phase, reason) in g.history.iter().rev().take(12) {
                let time = chrono::DateTime::parse_from_rfc3339(at)
                    .map(|t| {
                        t.with_timezone(&chrono::Local)
                            .format("%H:%M:%S")
                            .to_string()
                    })
                    .unwrap_or_else(|_| at.clone());
                let why = reason
                    .as_deref()
                    .filter(|r| !r.is_empty())
                    .map(|r| format!(" ({})", pause_text(r)))
                    .unwrap_or_default();
                md.push_str(&format!("- `{time}` {disposition} · {phase}{why}\n"));
            }
        }
        let waiting = self.prompts.iter().any(|p| matches!(p, Prompt::Goal(..)));
        let mut options = vec![];
        if waiting {
            options.push(
                DialogOption::new("review", "Open the waiting decision")
                    .key('o')
                    .describe("a draft, proposal or roadblock is waiting for you"),
            );
        }
        match g.disposition.as_str() {
            "running" => options.push(DialogOption::new("pause", "Pause").key('p')),
            "paused" if !g.awaiting_approval() => {
                options.push(DialogOption::new("resume", "Resume").key('r'));
            }
            _ => {}
        }
        if g.editable() {
            options.push(DialogOption::new("edit", "Edit the contract…").key('e'));
        }
        options.push(DialogOption::new("criteria", "Criteria checklist").key('v'));
        options.push(DialogOption::new("clear", "Clear the goal").key('x'));
        options.push(DialogOption::new("close", "Close").key('q'));
        let dialog = Dialog::new("Goal", Body::Markdown(md), options);
        self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::GoalStatus);
    }
}

/// The goal marker's colour by disposition (status line, /criteria).
pub(super) fn goal_style(g: &GoalView) -> Style {
    let t = crate::theme::theme();
    match g.disposition.as_str() {
        "running" => Style::default().fg(t.accent),
        "roadblocked" => Style::default().fg(t.red).add_modifier(Modifier::BOLD),
        "complete" => Style::default().fg(t.green),
        _ if g.awaiting_approval() => Style::default().fg(t.yellow).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(t.yellow),
    }
}

/// "◎ goal running · 2/5 met · review 1/3".
pub(super) fn goal_line(g: &GoalView) -> String {
    let (met, total, _) = g.contract.counts();
    let mut s = format!("◎ goal {}", g.state_text());
    if total > 0 {
        s.push_str(&format!(" · {met}/{total} met"));
    }
    if g.review_rounds > 0 {
        s.push_str(&format!(
            " · review {}/{}",
            g.review_rounds, g.max_review_rounds
        ));
    }
    s
}

/// A pause reason in words.
pub(super) fn pause_text(r: &str) -> String {
    match r {
        "user" => "paused by you".into(),
        "restart" => "the session was reopened; goals never resume by themselves".into(),
        "usage-limit" => "provider usage limit; /goal resume retries".into(),
        "no-progress" => "no progress for several turns".into(),
        "repeated-gaps" => "the review kept finding the same gaps".into(),
        "review-cap" => "the review round limit was reached".into(),
        "approval" => "waiting for your approval".into(),
        "draft-failed" => "drafting failed".into(),
        "review-failed" => "the review failed".into(),
        r if r.starts_with("limit:") => format!("limit {} reached", &r[6..]),
        r => r.into(),
    }
}
