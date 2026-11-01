//! Host-owned root /goal lifecycle; all policy is captured at creation.
use crate::{Action, Control, Session, State, goal::GoalContract, schema::*, sessions};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalLifecycle {
    pub at: String,
    pub disposition: String,
    pub phase: String,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalRun {
    pub id: String,
    pub contract: GoalContract,
    pub disposition: String,
    pub phase: String,
    pub pause_reason: Option<String>,
    pub policy: code_config::GoalConfig,
    pub model: String,
    pub effort: Option<String>,
    pub model_config: code_config::ModelConfig,
    pub turns: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    pub active_seconds: f64,
    pub no_progress_turns: u64,
    pub review_rounds: u64,
    pub gap_fingerprint: Option<String>,
    pub repeated_gap_rounds: u64,
    pub usage_resumes: u64,
    pub gaps: Vec<String>,
    pub next_step: Option<String>,
    pub history: Vec<GoalLifecycle>,
    pub baseline_tokens: u64,
    pub baseline_usd: f64,
    #[serde(default)]
    pub unknown_cost_calls: u64,
    #[serde(default)]
    pub baseline_unknown_cost_calls: u64,
    #[serde(default)]
    pub caching: code_config::PromptCaching,
}
impl GoalRun {
    pub(crate) fn transition(&mut self, disposition: &str, reason: Option<String>) {
        self.disposition = disposition.into();
        self.pause_reason = reason.clone();
        self.history.push(GoalLifecycle {
            at: sessions::now(),
            disposition: disposition.into(),
            phase: self.phase.clone(),
            reason,
        });
    }
    pub fn exit_code(&self) -> u8 {
        match self.disposition.as_str() {
            "complete" => 0,
            "roadblocked" => 3,
            "paused" if self.pause_reason.as_deref() == Some("usage-limit") => 75,
            "paused"
                if matches!(
                    self.pause_reason.as_deref(),
                    Some("error" | "draft-failed" | "review-failed")
                ) =>
            {
                1
            }
            "paused" | "cleared" => 4,
            _ => 1,
        }
    }
}
impl Control {
    pub(crate) fn goal_state(&self) -> Option<GoalRun> {
        self.journal.lock().unwrap().header.goal.clone()
    }
    pub(crate) fn save_goal(&self, mut run: GoalRun) -> Result<(), EngineError> {
        let totals = self.journal.lock().unwrap().totals.clone();
        run.tokens =
            (totals.input_tokens + totals.output_tokens).saturating_sub(run.baseline_tokens);
        run.cost_usd = (totals.known_cost_usd - run.baseline_usd).max(0.0);
        run.unknown_cost_calls = totals
            .unknown_cost_calls
            .saturating_sub(run.baseline_unknown_cost_calls);
        run.active_seconds += self.goal_clock.lock().unwrap().take();
        let envelope = {
            let mut j = self.journal.lock().unwrap();
            if let Some(current) = &j.header.goal {
                if current.id == run.id && current.history.len() > run.history.len() {
                    return Err(EngineError::bad(
                        "goal lifecycle changed while work was running",
                    ));
                }
                if current.id != run.id
                    && !matches!(current.disposition.as_str(), "cleared" | "complete")
                {
                    return Err(EngineError::bad("another goal is already active"));
                }
                if current.id == run.id {
                    run.active_seconds = run.active_seconds.max(current.active_seconds);
                    run.contract = j
                        .contracts
                        .get("root")
                        .cloned()
                        .unwrap_or_else(|| run.contract.clone());
                }
            }
            j.append(
                "root",
                HistoryEvent::GoalState {
                    state: Box::new(run.clone()),
                },
            )?
        };
        self.emit(EngineEvent::Committed { envelope });
        self.emit(EngineEvent::GoalUpdate {
            state: Box::new(run),
        });
        self.changed.notify_waiters();
        Ok(())
    }
    pub(crate) fn goal_roadblocked(&self) -> Result<(), EngineError> {
        if let Some(mut run) = self.goal_state().filter(|g| g.disposition == "running") {
            run.transition("roadblocked", None);
            self.save_goal(run)?;
        }
        Ok(())
    }
    pub(crate) fn pause_goal(&self, reason: &str) -> Result<(), EngineError> {
        if let Some(mut run) = self.goal_state()
            && matches!(run.disposition.as_str(), "running" | "paused")
        {
            run.transition("paused", Some(reason.into()));
            self.save_goal(run)?;
        }
        Ok(())
    }
    pub(crate) fn goal_limits(&self) -> Result<bool, EngineError> {
        let Some(mut run) = self.goal_state().filter(|r| r.disposition == "running") else {
            return Ok(false);
        };
        let totals = self.journal.lock().unwrap().totals.clone();
        run.tokens =
            (totals.input_tokens + totals.output_tokens).saturating_sub(run.baseline_tokens);
        run.cost_usd = (totals.known_cost_usd - run.baseline_usd).max(0.0);
        run.unknown_cost_calls = totals
            .unknown_cost_calls
            .saturating_sub(run.baseline_unknown_cost_calls);
        run.active_seconds += self.goal_clock.lock().unwrap().take();
        let l = &run.policy.limits;
        let reason = if l.max_tokens.is_some_and(|n| run.tokens >= n) {
            Some("limit:maxTokens")
        } else if l
            .max_usd
            .is_some_and(|n| run.cost_usd >= n || run.unknown_cost_calls > 0)
        {
            if run.unknown_cost_calls > 0 {
                run.next_step = Some("A call has unknown cost, so the USD limit cannot be enforced. Inspect /cost; clear and start with reported costs or without a USD limit.".into());
            }
            Some("limit:maxUsd")
        } else if l
            .max_minutes
            .is_some_and(|n| run.active_seconds >= n * 60.0)
        {
            Some("limit:maxMinutes")
        } else {
            None
        };
        if let Some(reason) = reason {
            run.transition("paused", Some(reason.into()));
        }
        self.save_goal(run)?;
        Ok(reason.is_some())
    }
}
impl Session {
    /// Whole-tree cost attributable to the evaluator and skeptic (display as "goal review").
    pub fn goal_review_totals(&self) -> SessionTotals {
        let j = self.control.journal.lock().unwrap();
        let mut total = SessionTotals::default();
        for history in j.histories.values() {
            for event in history {
                if let HistoryEvent::Usage { usage } = event
                    && usage.kind == "goal_review"
                {
                    total.add(usage);
                }
            }
        }
        total
    }
    /// Reconstruct pending dialogs after opening/resuming a session.
    pub fn goal_requests(&self) -> Vec<EngineEvent> {
        let j = self.control.journal.lock().unwrap();
        let mut events = Vec::new();
        if let Some(run) = &j.header.goal
            && run.phase == "draft"
            && run.disposition == "paused"
        {
            events.push(EngineEvent::GoalDraft {
                contract: run.contract.clone(),
            });
        }
        for (agent_id, contract) in &j.contracts {
            for proposal in contract.proposals.iter().filter(|p| p.status == "open") {
                if contract
                    .criteria()
                    .any(|c| c.id == proposal.criterion_id && c.owner == "user")
                {
                    events.push(EngineEvent::ProposalRequest {
                        agent_id: agent_id.clone(),
                        proposal: proposal.clone(),
                    });
                }
            }
            if agent_id == "root"
                && let Some(roadblock) = contract.roadblock()
            {
                events.push(EngineEvent::RoadblockRequest {
                    agent_id: agent_id.clone(),
                    roadblock: roadblock.clone(),
                });
            }
        }
        events
    }
    pub fn contract(&self, agent_id: &str) -> GoalContract {
        self.control.contract(
            &self
                .resolve_agent_id(agent_id)
                .unwrap_or_else(|_| agent_id.into()),
        )
    }
    pub fn goal_status(&self) -> Option<GoalRun> {
        self.control.goal_state()
    }
    pub fn goal_start(
        &self,
        text: impl Into<String>,
    ) -> Result<mpsc::UnboundedReceiver<EngineEvent>, EngineError> {
        if self.is_active() {
            return Err(EngineError::bad(
                "pause the current turn before starting a goal",
            ));
        }
        if self
            .goal_status()
            .is_some_and(|g| !matches!(g.disposition.as_str(), "cleared" | "complete"))
        {
            return Err(EngineError::bad(
                "one goal per session; clear the existing goal first",
            ));
        }
        let text = text.into();
        if text.trim().is_empty() || text.chars().count() > 4096 {
            return Err(EngineError::bad(
                "goal objective must contain 1–4096 characters",
            ));
        }
        {
            let mut state = self
                .state
                .try_lock()
                .map_err(|_| EngineError::bad("session is busy"))?;
            state.reload()?;
        }
        let snapshot = self.config.lock().unwrap().clone();
        let settings = self.settings();
        let model = crate::build_model(&snapshot, &settings)?;
        if model.command_latest() || model.command_text() || !model.config.capabilities.tools {
            return Err(EngineError::bad(
                "/goal requires a model with native tool calls so drafting and review restrictions can be enforced",
            ));
        }
        let policy = snapshot.config.goal.clone();
        if policy.no_progress_turns == 0
            || policy.repeated_gap_rounds == 0
            || policy.max_review_rounds == 0
            || policy.usage_resume.backoff_secs == 0
            || policy
                .limits
                .max_usd
                .is_some_and(|n| !n.is_finite() || n <= 0.0)
            || policy
                .limits
                .max_minutes
                .is_some_and(|n| !n.is_finite() || n <= 0.0)
        {
            return Err(EngineError::bad(
                "goal policy counts must be positive and monetary/time limits finite and positive",
            ));
        }
        let contract = GoalContract {
            objective: text.clone(),
            revision: self.contract("root").revision,
            ..GoalContract::default()
        };
        self.control.save_contract("root", contract.clone())?;
        let totals = self.totals();
        let mut run = GoalRun {
            id: uuid::Uuid::new_v4().to_string(),
            contract,
            disposition: "running".into(),
            phase: "drafting".into(),
            pause_reason: None,
            policy,
            model: model.id(),
            effort: model.effort.clone(),
            model_config: model.config.clone(),
            turns: 0,
            tokens: 0,
            cost_usd: 0.0,
            active_seconds: 0.0,
            no_progress_turns: 0,
            review_rounds: 0,
            gap_fingerprint: None,
            repeated_gap_rounds: 0,
            usage_resumes: 0,
            gaps: vec![],
            next_step: None,
            history: vec![],
            baseline_tokens: totals.input_tokens + totals.output_tokens,
            baseline_usd: totals.known_cost_usd,
            unknown_cost_calls: 0,
            baseline_unknown_cost_calls: totals.unknown_cost_calls,
            caching: model.provider.prompt_caching.clone(),
        };
        run.model_config.kind = Some(model.provider.kind);
        run.transition("running", None);
        self.control.save_goal(run)?;
        let generation = self.control.journal.lock().unwrap().header.title_generation;
        let title: String = text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .filter(|c| !c.is_control())
            .take(60)
            .collect();
        self.control.change_title(
            if title.is_empty() {
                "Goal session".into()
            } else {
                title
            },
            TitleSource::Auto,
            Some(generation),
        )?;
        Ok(self.launch(Action::GoalDraft(text)))
    }
    pub fn goal_approve(
        &self,
        mut contract: GoalContract,
    ) -> Result<mpsc::UnboundedReceiver<EngineEvent>, EngineError> {
        if self.is_active() {
            return Err(EngineError::bad(
                "wait for drafting TurnEnd before approving",
            ));
        }
        let mut run = self
            .goal_status()
            .ok_or_else(|| EngineError::bad("no goal draft"))?;
        if run.phase != "draft" {
            return Err(EngineError::bad("goal is not awaiting draft approval"));
        }
        if (contract.acceptance.is_empty() && contract.findings.is_empty())
            || contract.objective.trim().is_empty()
        {
            return Err(EngineError::bad(
                "approval requires an objective and acceptance or findings criteria",
            ));
        }
        contract.own("user");
        for c in contract.acceptance.iter_mut().chain(&mut contract.findings) {
            c.status = "open".into();
            c.evidence.clear();
            c.findings.clear();
            c.checked_clean = false;
            c.verification = "self-reported".into();
            c.override_note = None;
        }
        contract.validate()?;
        self.control.save_contract("root", contract.clone())?;
        run.contract = contract;
        run.phase = "executing".into();
        run.transition("running", None);
        self.control.save_goal(run)?;
        Ok(self.launch(Action::GoalContinue))
    }
    pub fn goal_cancel_draft(&self) -> Result<(), EngineError> {
        self.goal_clear()
    }
    pub fn goal_pause(&self) -> Result<(), EngineError> {
        self.control.pause_goal("user")?;
        self.cancel_active();
        Ok(())
    }
    pub fn goal_resume(&self) -> Result<mpsc::UnboundedReceiver<EngineEvent>, EngineError> {
        if self.is_active() {
            return Err(EngineError::bad(
                "wait for the current TurnEnd before resuming",
            ));
        }
        let mut run = self
            .goal_status()
            .ok_or_else(|| EngineError::bad("no goal"))?;
        if run.disposition != "paused" {
            return Err(EngineError::bad(
                "only paused goals may resume; resolve roadblocks separately",
            ));
        }
        if run.phase == "draft" {
            return Err(EngineError::bad("approve or cancel the goal draft"));
        }
        if run.phase == "drafting" {
            run.usage_resumes = 0;
            let text = run.contract.objective.clone();
            run.transition("running", None);
            self.control.save_goal(run)?;
            return Ok(self.launch(Action::GoalDraft(text)));
        }
        run.usage_resumes = 0;
        run.no_progress_turns = 0;
        run.phase = "executing".into();
        run.transition("running", None);
        self.control.save_goal(run)?;
        Ok(self.launch(Action::GoalContinue))
    }
    pub fn goal_edit(&self, mut contract: GoalContract) -> Result<(), EngineError> {
        let run = self
            .goal_status()
            .ok_or_else(|| EngineError::bad("no goal"))?;
        if !matches!(run.disposition.as_str(), "paused" | "roadblocked") && run.phase != "draft" {
            return Err(EngineError::bad(
                "pause the goal before editing its contract",
            ));
        }
        if (contract.acceptance.is_empty() && contract.findings.is_empty())
            || contract.objective.trim().is_empty()
        {
            return Err(EngineError::bad(
                "a goal needs an objective and at least one acceptance or findings criterion; use goal_clear to remove it",
            ));
        }
        let old = self.contract("root");
        let finding_ids: std::collections::BTreeSet<_> =
            contract.findings.iter().map(|c| c.id.clone()).collect();
        contract.own("user");
        for c in contract
            .acceptance
            .iter_mut()
            .chain(&mut contract.findings)
            .chain(&mut contract.rejection)
            .chain(&mut contract.non_goals)
            .chain(&mut contract.assumed_scope)
        {
            if old
                .criteria()
                .find(|prior| prior.id == c.id)
                .is_none_or(|prior| {
                    prior.text != c.text
                        || prior.check != c.check
                        || old.findings.iter().any(|f| f.id == c.id) != finding_ids.contains(&c.id)
                })
            {
                c.status = "open".into();
                c.evidence.clear();
                c.findings.clear();
                c.checked_clean = false;
                c.verification = "self-reported".into();
                c.override_note = None;
                c.updated = sessions::now();
            }
        }
        contract.validate()?;
        self.control.save_contract("root", contract)?;
        Ok(())
    }
    pub fn goal_clear(&self) -> Result<(), EngineError> {
        self.cancel_active();
        if let Some(mut run) = self.goal_status() {
            run.transition("cleared", None);
            self.control.save_goal(run)?;
        }
        self.control.save_contract(
            "root",
            GoalContract {
                revision: self.contract("root").revision,
                ..GoalContract::default()
            },
        )?;
        Ok(())
    }
    pub fn resolve_proposal(
        &self,
        agent_id: &str,
        proposal_id: &str,
        resolution: &str,
        text: Option<String>,
    ) -> Result<(), EngineError> {
        let agent_id = self.resolve_agent_id(agent_id)?;
        let agent_id = agent_id.as_str();
        let c = self.contract(agent_id);
        let p = c
            .proposals
            .iter()
            .find(|p| p.id == proposal_id)
            .ok_or_else(|| EngineError::bad("proposal not found"))?;
        if !c
            .criteria()
            .any(|c| c.id == p.criterion_id && c.owner == "user")
        {
            return Err(EngineError::bad(
                "front end resolves only user-owned proposals",
            ));
        }
        self.control
            .resolve_proposal(agent_id, proposal_id, resolution, text)
    }
    pub fn resolve_roadblock(
        &self,
        roadblock_id: &str,
        resolution: &str,
        text: Option<String>,
        mut contract: Option<GoalContract>,
    ) -> Result<Option<mpsc::UnboundedReceiver<EngineEvent>>, EngineError> {
        if self.is_active() {
            return Err(EngineError::bad(
                "wait for roadblocked TurnEnd before resolving",
            ));
        }
        if let Some(c) = &mut contract {
            c.own("user");
            c.validate()?;
        }
        self.control
            .resolve_roadblock("root", roadblock_id, resolution, text.clone(), contract)?;
        if let Some(mut run) = self.goal_status() {
            run.contract = self.contract("root");
            if resolution == "stop" {
                run.transition("roadblocked", Some("stop".into()));
                self.control.save_goal(run)?;
                return Ok(None);
            }
            run.phase = "executing".into();
            run.transition("running", None);
            self.control.save_goal(run)?;
            return Ok(Some(self.launch(Action::GoalContinue)));
        }
        Ok(if resolution == "stop" {
            None
        } else {
            Some(self.launch(Action::Send(
                crate::injection::notice(format!(
                    "Roadblock resolved: {resolution}. {}",
                    text.unwrap_or_default()
                )),
                vec![],
            )))
        })
    }
    pub(crate) fn cancel_active(&self) {
        let c = self.coordinator.lock().unwrap();
        if let Some(t) = &c.active {
            t.cancel.cancel();
            t.barrier.revoke();
        }
    }
}
fn continuation(run: &GoalRun) -> String {
    format!(
        "Continue the active goal. Work until its acceptance or findings criteria are met with evidence; run checks through criteria.met. Respect non-goals and raise a roadblock rather than silently deviating. Contract status: {}\nUnresolved gaps (up to 8): {}\nNext step from the last review: {}",
        serde_json::to_string(&run.contract).unwrap(),
        json!(run.gaps.iter().take(8).collect::<Vec<_>>()),
        run.next_step
            .as_deref()
            .unwrap_or("Address the first open acceptance or findings criterion.")
    )
}
pub(crate) async fn drive(
    state: &mut State,
    action: Action,
    tx: &crate::model::EventSender,
    cancel: &CancellationToken,
    barrier: Arc<crate::tools::CommitBarrier>,
) -> Result<String, EngineError> {
    if let Action::GoalDraft(text) = action {
        let directive = format!(
            "Draft a criteria contract for this objective: {text}\nThe user explicitly chose /goal, so criteria are required even for a small objective; ordinary trivial tasks do not need criteria. This is a READ-ONLY planning turn. You may use read/grep/glob/history/models and script containing those calls plus criteria. Record the objective and add acceptance/findings/rejection/nonGoal/assumedScope/step criteria with criteria actions. Choose findings criteria for investigation/review areas: completion needs evidence per finding, or checked-clean with evidence. Choose acceptance with check {{command,cwd?,timeoutSecs?}} when an executable check fits. Do not implement the task, run bash, mark criteria met, delegate, or write files. Finish once the draft is recorded for owner approval."
        );
        let mut prompt = Some((crate::injection::notice(&directive), vec![]));
        let result = loop {
            state.control.goal_clock.lock().unwrap().start();
            let result = crate::run(state, prompt.take(), tx, cancel, barrier.clone()).await;
            state.control.goal_clock.lock().unwrap().stop();
            state.control.goal_limits()?;
            if state
                .control
                .goal_state()
                .is_some_and(|g| g.disposition != "running")
            {
                if let Some(run) = state.control.goal_state() {
                    state.control.save_goal(run)?;
                }
                return if cancel.is_cancelled() {
                    result
                } else {
                    Ok("goal-paused".into())
                };
            }
            if let Err(error) = &result
                && usage_limit(error)
            {
                if usage_backoff(&state.control, cancel).await? {
                    continue;
                }
                return Ok("goal-usage-limit: run /goal resume".into());
            }
            break result;
        };
        if result.is_ok() && !cancel.is_cancelled() {
            let contract = state.control.contract("root");
            contract.validate()?;
            if contract.acceptance.is_empty() && contract.findings.is_empty() {
                state.control.pause_goal("draft-failed")?;
                return Err(EngineError::bad(
                    "draft has no acceptance or findings criteria",
                ));
            }
            let mut run = state.control.goal_state().unwrap();
            run.contract = contract.clone();
            run.phase = "draft".into();
            run.transition("paused", Some("approval".into()));
            state.control.save_goal(run)?;
            state.control.emit(EngineEvent::GoalDraft { contract });
        } else if !cancel.is_cancelled() {
            state.control.pause_goal("draft-failed")?;
        }
        return result;
    }
    let mut next = match action {
        Action::Send(s, a) => Some((s, a)),
        Action::Compact(f) => return crate::compact(state, f, tx, cancel).await,
        Action::Prune => return crate::prune::apply(state, cancel, barrier),
        Action::IdleCompact => {
            let result = crate::compact(state, None, tx, cancel).await?;
            state.control.emit(EngineEvent::IdleCompacted);
            return Ok(result);
        }
        Action::Queued => {
            state.deliver_queue(true)?;
            None
        }
        _ => None,
    };
    if !state
        .control
        .goal_state()
        .is_some_and(|g| g.disposition == "running")
    {
        return crate::run(state, next, tx, cancel, barrier).await;
    }
    loop {
        let goal = state
            .control
            .goal_state()
            .filter(|g| g.disposition == "running" && g.phase != "drafting");
        if goal.is_some() {
            if state.control.goal_limits()? {
                return Ok("goal-paused".into());
            }
            let mut g = state.control.goal_state().unwrap();
            g.turns += 1;
            state.control.save_goal(g.clone())?;
            if next.is_none() {
                state.control.commit(
                    "root",
                    HistoryEvent::SystemReminder {
                        text: continuation(&g),
                    },
                )?;
            }
        }
        let before = state
            .control
            .goal_progress
            .load(std::sync::atomic::Ordering::Relaxed);
        state.control.goal_clock.lock().unwrap().start();
        let result = crate::run(state, next.take(), tx, cancel, barrier.clone()).await;
        state.control.goal_clock.lock().unwrap().stop();
        let Some(mut g) = state
            .control
            .goal_state()
            .filter(|g| g.disposition == "running" || g.disposition == "roadblocked")
        else {
            if let Some(g) = state.control.goal_state() {
                state.control.save_goal(g)?;
            }
            return result;
        };
        state.control.save_goal(g.clone())?;
        if cancel.is_cancelled() {
            state.control.pause_goal("user")?;
            return Err(EngineError::cancelled());
        }
        if g.disposition == "roadblocked" || state.control.contract("root").roadblock().is_some() {
            return Ok("roadblocked".into());
        }
        if let Err(error) = result {
            if usage_limit(&error) {
                if usage_backoff(&state.control, cancel).await? {
                    continue;
                }
                return Ok("goal-usage-limit: run /goal resume".into());
            }
            state.control.pause_goal("error")?;
            return Err(error);
        }
        state.control.goal_limits()?;
        g = state.control.goal_state().unwrap();
        if g.disposition != "running" {
            return Ok("goal-paused".into());
        }
        if before
            == state
                .control
                .goal_progress
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            g.no_progress_turns += 1;
        } else {
            g.no_progress_turns = 0;
        }
        g.contract = state.control.contract("root");
        if g.contract.complete() {
            if state.control.runtime.has_unfinished() || state.control.root_mail_pending() {
                // A completed launch criterion does not discard running work.
                // Park without model calls until a result can continue the root.
                loop {
                    let changed = state.control.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if state.control.root_mail_pending() || !state.control.runtime.has_unfinished()
                    {
                        break;
                    }
                    tokio::select! {
                        _ = &mut changed => {},
                        _ = cancel.cancelled() => return Err(EngineError::cancelled()),
                    }
                }
                continue;
            }
            if !g.policy.review_panel || !g.contract.has_completion_criteria() {
                g.phase = "done".into();
                g.transition("complete", None);
                state.control.save_goal(g)?;
                return Ok("goal-complete".into());
            }
            if g.review_rounds >= g.policy.max_review_rounds {
                g.transition("paused", Some("review-cap".into()));
                state.control.save_goal(g)?;
                return Ok("goal-paused".into());
            }
            g.phase = "reviewing".into();
            g.review_rounds += 1;
            state.control.save_goal(g.clone())?;
            state.control.goal_clock.lock().unwrap().start();
            let review = crate::goal_review::panel(state, cancel, barrier.clone()).await;
            state.control.goal_clock.lock().unwrap().stop();
            state.control.goal_limits()?;
            let mut g = state.control.goal_state().unwrap();
            if g.disposition != "running" {
                return Ok("goal-paused".into());
            }
            let (gaps, next_step) = match review {
                Ok(v) => v,
                Err(e) if usage_limit(&e) => {
                    g.review_rounds = g.review_rounds.saturating_sub(1);
                    g.phase = "executing".into();
                    state.control.save_goal(g)?;
                    if usage_backoff(&state.control, cancel).await? {
                        continue;
                    }
                    return Ok("goal-usage-limit: run /goal resume".into());
                }
                Err(e) => {
                    state.control.pause_goal("review-failed")?;
                    return Err(e);
                }
            };
            g.gaps = gaps;
            g.next_step = next_step;
            if g.gaps.is_empty() {
                g.phase = "done".into();
                g.transition("complete", None);
                state.control.save_goal(g)?;
                return Ok("goal-complete".into());
            }
            let mut normalized = g
                .gaps
                .iter()
                .map(|s| {
                    s.to_lowercase()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>();
            normalized.sort();
            normalized.dedup();
            let fingerprint = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&normalized).unwrap())
            );
            g.repeated_gap_rounds = if g.gap_fingerprint.as_deref() == Some(&fingerprint) {
                g.repeated_gap_rounds + 1
            } else {
                1
            };
            g.gap_fingerprint = Some(fingerprint);
            if g.repeated_gap_rounds >= g.policy.repeated_gap_rounds {
                g.transition("paused", Some("repeated-gaps".into()));
            } else if g.review_rounds >= g.policy.max_review_rounds {
                g.transition("paused", Some("review-cap".into()));
            }
            g.phase = "executing".into();
            state.control.save_goal(g.clone())?;
        }
        if g.disposition == "running" && g.no_progress_turns >= g.policy.no_progress_turns {
            g.transition("paused", Some("no-progress".into()));
        }
        state.control.save_goal(g.clone())?;
        if g.disposition != "running" {
            return Ok("goal-paused".into());
        }
        state.deliver_queue(true)?;
    }
}
fn usage_limit(e: &EngineError) -> bool {
    let m = e.message.to_lowercase();
    e.kind == ErrorKind::RateLimit
        || [
            "rate limited",
            "usage limit",
            "usage_limit",
            "insufficient_quota",
            "quota exceeded",
            "credit balance",
            "budget exhausted",
        ]
        .iter()
        .any(|s| m.contains(s))
}

/// Runtime-only monotonic clock. Owner waits and provider backoff never consume active time.
#[derive(Default)]
pub(crate) struct GoalClock {
    running: bool,
    since: Option<Instant>,
    elapsed: f64,
    waits: std::collections::BTreeSet<String>,
}
impl GoalClock {
    fn accrue(&mut self) {
        if let Some(at) = self.since.take() {
            self.elapsed += at.elapsed().as_secs_f64();
        }
    }
    pub(crate) fn start(&mut self) {
        self.running = true;
        if self.waits.is_empty() && self.since.is_none() {
            self.since = Some(Instant::now());
        }
    }
    pub(crate) fn stop(&mut self) {
        self.accrue();
        self.running = false;
    }
    fn take(&mut self) -> f64 {
        self.accrue();
        let value = std::mem::take(&mut self.elapsed);
        if self.running && self.waits.is_empty() {
            self.since = Some(Instant::now());
        }
        value
    }
    pub(crate) fn wait(&mut self, key: String) {
        self.accrue();
        self.waits.insert(key);
    }
    pub(crate) fn resume(&mut self, key: &str) {
        self.waits.remove(key);
        if self.waits.is_empty() && self.running {
            self.since = Some(Instant::now());
        }
    }
}

pub(crate) fn mutating_call(call: &ToolCall, cwd: &std::path::Path) -> bool {
    match call.name.as_str() {
        "write" | "edit" | "task" | "generate" => true,
        "agents" => matches!(call.arguments["action"].as_str(), Some("stop" | "finalize")),
        "decide" => true,
        "bash" => !call.arguments["command"]
            .as_str()
            .is_some_and(|c| crate::permissions::static_read_only(c, cwd)),
        name => name.starts_with("mcp."),
    }
}

async fn usage_backoff(control: &Control, cancel: &CancellationToken) -> Result<bool, EngineError> {
    let Some(mut g) = control.goal_state().filter(|g| g.disposition == "running") else {
        return Ok(false);
    };
    if g.usage_resumes >= g.policy.usage_resume.max {
        g.transition("paused", Some("usage-limit".into()));
        control.save_goal(g)?;
        return Ok(false);
    }
    let phase = g.phase.clone();
    let secs = g
        .policy
        .usage_resume
        .backoff_secs
        .saturating_mul(
            1u64.checked_shl(g.usage_resumes.min(63) as u32)
                .unwrap_or(u64::MAX),
        )
        .min(300);
    g.usage_resumes += 1;
    g.phase = "usage-backoff".into();
    control.save_goal(g)?;
    tokio::select! {_=tokio::time::sleep(Duration::from_secs(secs))=>{},_=cancel.cancelled()=>{control.pause_goal("user")?;return Err(EngineError::cancelled());}}
    let Some(mut g) = control.goal_state().filter(|g| g.disposition == "running") else {
        return Ok(false);
    };
    g.phase = phase;
    control.save_goal(g)?;
    Ok(true)
}
