//! One ACP session: an engine `Session`, its prompt queue, deferred settings
//! and the event loop that turns engine events into `session/update`s,
//! permission requests and question forms.

use crate::mapping::{self, clip, text_block, text_content};
use crate::rpc::{Out, RpcError, conforms};
use code_config::{ConfigStore, Modality, PermissionMode};
use code_engine::Session;
use code_engine::schema::{
    ApprovalDecision, ApprovalRequest, EngineEvent, HistoryEvent, QuestionKind, QuestionRecord,
    QuestionSpec,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Default)]
pub struct ClientCaps {
    /// `clientCapabilities.elicitation.form`: structured question forms.
    pub elicitation_form: bool,
}

#[derive(Clone)]
pub enum Action {
    Send(String, Vec<code_engine::schema::Attachment>),
    Compact(Option<String>),
    Prune,
    Guidance,
    Decisions(String),
    Subagents(String),
    Goal(String),
}

/// Settings chosen before the first prompt. Applying them would create the
/// session on disk (D44), so they wait for the first prompt.
#[derive(Default)]
struct Pending {
    mode: Option<PermissionMode>,
    model: Option<String>,
    effort: Option<Option<String>>,
    safety: Option<String>,
}

#[derive(Default)]
struct State {
    inflight: Option<Value>,
    queue: VecDeque<(Value, Action)>,
    /// Mid-turn prompts held by the engine queue (D73): (queue id, request).
    engine_queued: Vec<(u64, Value)>,
    /// Prompts the engine delivered into the running turn (steer,
    /// interrupt): answered when that turn ends.
    attached: Vec<Value>,
    cancel_requested: bool,
    last_error: Option<String>,
    materialized: bool,
    pending: Pending,
    asked: HashSet<String>,
}

pub struct AcpSession {
    /// The id the client uses. After a `session/load` fork it names the source
    /// session while `session` is the fork.
    pub acp_id: String,
    pub session: Session,
    pub store: ConfigStore,
    pub out: Arc<Out>,
    caps: ClientCaps,
    state: Mutex<State>,
    /// Ends the event loop after `session/close`.
    stop: tokio::sync::Notify,
}

pub const MODES: [(PermissionMode, &str, &str, &str); 4] = [
    (
        PermissionMode::Manual,
        "manual",
        "Manual",
        "Ask before bash, scripts and outside-cwd access",
    ),
    (
        PermissionMode::Auto,
        "auto",
        "Auto",
        "The safety model approves; unsafe calls ask you",
    ),
    (
        PermissionMode::Unattended,
        "unattended",
        "Unattended",
        "The safety model decides; unsafe calls are rejected",
    ),
    (
        PermissionMode::Yolo,
        "yolo",
        "Yolo",
        "Run everything without asking",
    ),
];

pub fn mode_id(mode: PermissionMode) -> &'static str {
    MODES
        .iter()
        .find(|m| m.0 == mode)
        .map(|m| m.1)
        .unwrap_or("manual")
}
pub fn parse_mode(id: &str) -> Option<PermissionMode> {
    MODES.iter().find(|m| m.1 == id).map(|m| m.0)
}

/// ACP stop reason for an engine `TurnEnd`.
fn stop_reason(engine: &str) -> &'static str {
    match engine {
        "max_tokens" => "max_tokens",
        "refusal" | "content_filter" => "refusal",
        "cancelled" => "cancelled",
        _ => "end_turn",
    }
}

impl AcpSession {
    /// Wrap an opened session and start its event loop. `materialized` is
    /// false for a fresh `session/new` session that has nothing on disk yet.
    pub fn start(
        acp_id: String,
        session: Session,
        store: ConfigStore,
        out: Arc<Out>,
        caps: ClientCaps,
        materialized: bool,
        initial_mode: Option<PermissionMode>,
    ) -> Arc<Self> {
        let rx = session.subscribe();
        let health = session.clone();
        tokio::spawn(async move {
            health.mcp_health_check().await;
        });
        let this = Arc::new(Self {
            acp_id,
            session,
            store,
            out,
            caps,
            stop: tokio::sync::Notify::new(),
            state: Mutex::new(State {
                materialized,
                ..Default::default()
            }),
        });
        if let Some(mode) = initial_mode {
            let _ = this.set_mode(mode);
        }
        tokio::spawn(this.clone().events(rx));
        this
    }

    fn decisions_command(&self, command: &str) -> Result<Value, code_engine::schema::EngineError> {
        let mut words = command
            .splitn(3, char::is_whitespace)
            .filter(|s| !s.is_empty());
        let action = words.next().unwrap_or("list");
        if action == "list" {
            return self
                .session
                .search_decisions(json!({"status":"open","limit":100}));
        }
        if action == "search" {
            return self.session.search_decisions(json!({"action":"search","query":command.strip_prefix("search").unwrap_or("").trim(),"status":"any","limit":100}));
        }
        let id = words
            .next()
            .ok_or_else(|| code_engine::schema::EngineError::bad("decision id required"))?;
        let text = words
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let review = match action {
            "keep" => code_engine::DecisionReview::Keep,
            "change" => code_engine::DecisionReview::Change {
                steering: text.ok_or_else(|| {
                    code_engine::schema::EngineError::bad("change requires steering")
                })?,
            },
            "discuss" => code_engine::DecisionReview::Discuss { question: text },
            _ => {
                return Err(code_engine::schema::EngineError::bad(
                    "use /decisions list|search|keep|change|discuss",
                ));
            }
        };
        let fork_id = self.session.review_decision(id, review)?;
        Ok(json!({"id":id,"forkId":fork_id,"openCount":self.session.decision_open_count()}))
    }

    // ---- prompts ---------------------------------------------------------

    /// Start a prompt. A prompt that arrives mid-turn goes to the engine
    /// queue with config `queueDefault` (D73): next turn, steer or interrupt.
    /// It is answered when the turn that consumed it ends. Compactions,
    /// prompts behind other local ones and prompts after a cancel wait
    /// locally for the turn to end.
    pub fn prompt(self: &Arc<Self>, id: Value, action: Action) {
        if let Action::Subagents(command) = &action {
            let result = if command.is_empty() {
                Ok(())
            } else {
                self.set_config("subagents", command)
            };
            match result {
                Ok(()) => {
                    self.announce_settings();
                    self.message(&format!(
                        "Subagents: {}{}\n",
                        if self.session.subagents() == code_engine::SubagentMode::Flat {
                            "flat"
                        } else {
                            "tree"
                        },
                        if self.session.subagents_locked() {
                            " (fixed for this session)"
                        } else {
                            " (change before the first message)"
                        }
                    ));
                    self.out.respond(&id, json!({"stopReason":"end_turn"}));
                }
                Err(error) => self.out.fail(&id, error),
            }
            return;
        }
        if let Action::Decisions(command) = &action {
            match self.decisions_command(command) {
                Ok(value) => {
                    self.message(&serde_json::to_string_pretty(&value).unwrap());
                    self.out.respond(&id,json!({"stopReason":"end_turn","_meta":{"cockpit-code":{"openDecisions":self.session.open_decisions(),"decisionOpenCount":self.session.decision_open_count()}}}));
                }
                Err(e) => self.out.fail(&id, RpcError::params(e.message)),
            }
            return;
        }
        if matches!(action, Action::Guidance) {
            self.message(&self.session.guidance_report());
            self.out.respond(&id, json!({"stopReason":"end_turn"}));
            return;
        }
        if let Action::Goal(command) = &action {
            let handled = match command.as_str() {
                "pause" => Some(self.session.goal_pause()),
                "clear" => Some(self.session.goal_clear()),
                "" | "status" => {
                    self.message(
                        &serde_json::to_string_pretty(&self.session.goal_status()).unwrap(),
                    );
                    Some(Ok(()))
                }
                _ => None,
            };
            if let Some(result) = handled {
                match result {
                    Ok(()) => self.out.respond(&id, json!({"stopReason":"end_turn"})),
                    Err(e) => self.out.fail(&id, RpcError::internal(e.message)),
                };
                return;
            }
        }
        let mut st = self.state.lock().unwrap();
        if st.inflight.is_some() || self.session.is_active() {
            if let Action::Send(text, attachments) = &action
                && st.queue.is_empty()
                && !st.cancel_requested
                && st.materialized
            {
                let class = self.store.snapshot().config.queue_default;
                if let Ok(qid) = self.session.queue_message_with_attachments(
                    "root",
                    text.clone(),
                    attachments.clone(),
                    class,
                ) {
                    st.engine_queued.push((qid, id));
                    return;
                }
            }
            st.queue.push_back((id, action));
            return;
        }
        self.start_locked(&mut st, id, action);
        drop(st);
        self.announce_settings();
    }

    fn start_locked(&self, st: &mut State, id: Value, action: Action) {
        if !st.materialized {
            st.materialized = true;
            let pending = std::mem::take(&mut st.pending);
            let applied = [
                pending.model.map(|m| self.session.set_model(m)),
                pending.effort.map(|e| self.session.set_effort(e)),
                pending.safety.map(|s| self.session.set_safety_model(s)),
                pending.mode.map(|m| self.session.set_mode(m)),
            ];
            for e in applied.into_iter().flatten().filter_map(Result::err) {
                self.message(&format!("⚠ setting not applied: {}\n\n", e.message));
            }
        }
        let mut rx = match action.clone() {
            Action::Send(text, attachments) => {
                self.session.send_with_attachments(text, attachments)
            }
            Action::Compact(focus) => self.session.compact(focus),
            Action::Decisions(_) => unreachable!("local decisions command handled before queuing"),
            Action::Subagents(_) => unreachable!("local subagents command handled before queuing"),
            Action::Guidance => {
                self.message(&self.session.guidance_report());
                self.out.respond(&id, json!({"stopReason":"end_turn"}));
                return;
            }
            Action::Prune => {
                match self.session.prune_preview() {
                    Ok(report) => self.message(&format!("{}\n", report.confirmation)),
                    Err(e) => {
                        self.out.fail(&id, RpcError::internal(e.message));
                        return;
                    }
                }
                self.session.prune()
            }
            Action::Goal(command) => {
                let result = if command == "resume" {
                    self.session.goal_resume()
                } else {
                    self.session.goal_start(command)
                };
                match result {
                    Ok(rx) => rx,
                    Err(e) => {
                        self.out.fail(&id, RpcError::internal(e.message));
                        return;
                    }
                }
            }
        };
        // A competing turn (an engine-started answer turn) is refused
        // synchronously; such a prompt waits for that turn to end.
        if let Ok(EngineEvent::Error { error }) = rx.try_recv() {
            if error.message.contains("already active") {
                st.queue.push_front((id, action));
                return;
            }
            self.out.fail(&id, RpcError::internal(error.message));
            return;
        }
        st.inflight = Some(id);
        st.cancel_requested = false;
        st.last_error = None;
        // Dropping the per-action receiver would cancel the turn.
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
    }

    /// Stop forwarding engine events (the session was closed).
    pub fn stop(&self) {
        self.stop.notify_one();
    }

    pub fn cancel(&self) {
        self.cancel_pending(true);
    }
    pub(crate) fn prepare_close(&self) {
        self.cancel_pending(self.session.is_active());
    }
    fn cancel_pending(&self, suspend_mail: bool) {
        let mut st = self.state.lock().unwrap();
        st.cancel_requested = st.inflight.is_some();
        for (id, _) in st.queue.drain(..) {
            self.out.respond(&id, json!({"stopReason": "cancelled"}));
        }
        // Undelivered queued prompts are withdrawn; delivered ones end with
        // the cancelled turn.
        for (qid, id) in std::mem::take(&mut st.engine_queued) {
            let _ = self.session.unqueue(qid);
            self.out.respond(&id, json!({"stopReason": "cancelled"}));
        }
        drop(st);
        if suspend_mail {
            self.session.cancel();
        }
    }

    /// The engine delivered queued input (D73): a mid-turn prompt now
    /// belongs to the running turn, or starts the engine's next turn.
    fn delivered(&self, queue_id: u64) {
        let mut st = self.state.lock().unwrap();
        let Some(i) = st.engine_queued.iter().position(|(q, _)| *q == queue_id) else {
            return;
        };
        let (_, id) = st.engine_queued.remove(i);
        if st.inflight.is_none() {
            st.inflight = Some(id);
            st.cancel_requested = false;
            st.last_error = None;
        } else {
            st.attached.push(id);
        }
    }

    fn turn_ended(&self, engine_reason: &str) {
        let open_decisions = self.session.open_decisions();
        if !open_decisions.is_empty() {
            let rows = open_decisions
                .iter()
                .map(|r| {
                    format!(
                        "- {} [{}] {} → {} ({})",
                        r.id, r.deciding_agent, r.question, r.choice, r.reason
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.message(&format!(
                "\nOpen decisions ({}):\n{rows}\n",
                open_decisions.len()
            ));
        }
        let mut st = self.state.lock().unwrap();
        let attached = std::mem::take(&mut st.attached);
        if let Some(id) = st.inflight.take() {
            let error = (engine_reason == "error" && !st.cancel_requested)
                .then(|| st.last_error.take().unwrap_or_else(|| "turn failed".into()));
            for id in std::iter::once(id).chain(attached) {
                if st.cancel_requested {
                    self.out.respond(&id, json!({"stopReason": "cancelled"}));
                } else if let Some(message) = &error {
                    self.out.fail(&id, RpcError::internal(message.clone()));
                } else {
                    self.out
                        .respond(&id, json!({"stopReason": stop_reason(engine_reason),"_meta":{"cockpit-code":{"openDecisions":open_decisions,"decisionOpenCount":open_decisions.len()}}}));
                }
            }
            st.cancel_requested = false;
        }
        if !self.session.is_active()
            && let Some((id, action)) = st.queue.pop_front()
        {
            self.start_locked(&mut st, id, action);
        }
    }

    // ---- settings ----------------------------------------------------------

    pub fn mode(&self) -> PermissionMode {
        let st = self.state.lock().unwrap();
        st.pending
            .mode
            .or(self.session.settings().mode)
            .unwrap_or(self.store.snapshot().config.permissions.mode)
    }

    pub fn set_mode(&self, mode: PermissionMode) -> Result<(), RpcError> {
        if let Some(ex) = self.session.exclusive()
            && ex.safety_model().is_none()
            && matches!(mode, PermissionMode::Auto | PermissionMode::Unattended)
        {
            return Err(RpcError::params(format!(
                "{} needs a safety model, and this session is exclusive without one: manual and yolo only",
                mode_id(mode)
            )));
        }
        let mut st = self.state.lock().unwrap();
        if st.materialized {
            drop(st);
            self.session
                .set_mode(mode)
                .map_err(|e| RpcError::params(e.message))
        } else {
            st.pending.mode = Some(mode);
            Ok(())
        }
    }

    pub fn model(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        st.pending
            .model
            .clone()
            .or(self.session.settings().model)
            .or_else(|| self.store.snapshot().config.default_model.clone())
    }

    fn effort(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        match &st.pending.effort {
            Some(e) => e.clone(),
            None => self.session.settings().effort,
        }
    }

    fn safety(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        st.pending
            .safety
            .clone()
            .or_else(|| self.session.safety_model())
    }

    /// Chat-capable models of enabled providers (`provider/model`, label).
    fn models(&self) -> Vec<(String, String)> {
        let exclusive = self.session.exclusive().map(|e| e.model);
        let mut out = vec![];
        for (pid, provider) in &self.session.configuration().providers {
            if !provider.enabled {
                continue;
            }
            for (mid, m) in &provider.models {
                let id = format!("{pid}/{mid}");
                let text = m.capabilities.output.is_empty()
                    || m.capabilities.output.contains(&Modality::Text);
                if !text || exclusive.as_ref().is_some_and(|e| e != &id) {
                    continue;
                }
                let label = format!(
                    "{} · {}",
                    provider.label.clone().unwrap_or_else(|| pid.clone()),
                    m.label.clone().unwrap_or_else(|| mid.clone())
                );
                out.push((id, label));
            }
        }
        out
    }

    fn efforts(&self, model: Option<&str>) -> Vec<String> {
        let Some((pid, mid)) = model.and_then(|m| m.split_once('/')) else {
            return vec![];
        };
        self.store
            .snapshot()
            .providers
            .get(pid)
            .and_then(|p| p.models.get(mid))
            .map(|m| m.reasoning.efforts.clone())
            .unwrap_or_default()
    }

    /// ACP session modes: the four permission modes.
    pub fn modes(&self) -> Value {
        json!({
            "currentModeId": mode_id(self.mode()),
            "availableModes": MODES.iter().map(|(_, id, name, desc)| json!({
                "id": id, "name": name, "description": desc,
            })).collect::<Vec<_>>(),
        })
    }

    /// ACP session config options: mode, model, reasoning effort and the
    /// session's safety model (D45).
    pub fn config_options(&self) -> Value {
        let models = self.models();
        let model = self.model();
        let select = |id: &str,
                      name: &str,
                      desc: &str,
                      category: Option<&str>,
                      current: &str,
                      options: Vec<Value>| {
            let mut v = json!({
                "id": id, "name": name, "description": desc, "type": "select",
                "currentValue": current, "options": options,
            });
            if let Some(c) = category {
                v["category"] = json!(c);
            }
            v
        };
        let model_options = |extra: Option<(&str, &str)>| {
            let mut v: Vec<Value> = extra
                .map(|(id, name)| json!({"value": id, "name": name}))
                .into_iter()
                .collect();
            v.extend(
                models
                    .iter()
                    .map(|(id, name)| json!({"value": id, "name": name})),
            );
            v
        };
        let mut options = vec![select(
            "mode",
            "Permissions",
            "Who approves tool calls; applies to every agent in the tree",
            Some("mode"),
            mode_id(self.mode()),
            MODES
                .iter()
                .map(|(_, id, name, desc)| json!({"value": id, "name": name, "description": desc}))
                .collect(),
        )];
        let current = self.session.subagents();
        let modes = if self.session.subagents_locked() {
            vec![current]
        } else {
            vec![
                code_engine::SubagentMode::Tree,
                code_engine::SubagentMode::Flat,
            ]
        };
        options.push(select(
            "subagents",
            "Subagents",
            "Tree or flat; fixed after the first message",
            None,
            if current == code_engine::SubagentMode::Flat {
                "flat"
            } else {
                "tree"
            },
            modes
                .into_iter()
                .map(|mode| {
                    if mode == code_engine::SubagentMode::Flat {
                        json!({"value":"flat","name":"Flat (one layer)"})
                    } else {
                        json!({"value":"tree","name":"Tree"})
                    }
                })
                .collect(),
        ));
        let current_model = model.clone().unwrap_or_default();
        let listed = models.iter().any(|(id, _)| *id == current_model);
        options.push(select(
            "model",
            "Model",
            "Model for the root agent's next turn",
            Some("model"),
            &current_model,
            model_options((!listed).then_some((current_model.as_str(), "(not configured)"))),
        ));
        let efforts = self.efforts(model.as_deref());
        if !efforts.is_empty() {
            let mut opts = vec![json!({"value": "default", "name": "Default"})];
            opts.extend(efforts.iter().map(|e| json!({"value": e, "name": e})));
            options.push(select(
                "effort",
                "Reasoning effort",
                "Reasoning effort for the active model",
                Some("thought_level"),
                self.effort().as_deref().unwrap_or("default"),
                opts,
            ));
        }
        let safety = self.safety().unwrap_or_default();
        let none = safety.is_empty().then_some(("", "(none)"));
        options.push(select(
            "safety_model",
            "Safety model",
            "Reviews tool calls in auto and unattended modes, for the whole tree",
            None,
            &safety,
            model_options(none),
        ));
        Value::Array(options)
    }

    pub async fn set_config_ready(&self, id: &str, value: &str) -> Result<(), RpcError> {
        if matches!(id, "model" | "safety_model") {
            self.session
                .await_model(value)
                .await
                .map_err(|e| RpcError::params(e.message))?;
        }
        self.set_config(id, value)
    }

    pub fn set_config(&self, id: &str, value: &str) -> Result<(), RpcError> {
        if id == "subagents" {
            let mode = match value {
                "tree" => code_engine::SubagentMode::Tree,
                "flat" => code_engine::SubagentMode::Flat,
                _ => return Err(RpcError::params("subagents must be tree or flat")),
            };
            return self
                .session
                .set_subagents(mode)
                .map_err(|e| RpcError::params(e.message));
        }
        let materialized = self.state.lock().unwrap().materialized;
        let known = |v: &str| self.models().iter().any(|(id, _)| id == v);
        match id {
            "mode" => {
                let mode = parse_mode(value).ok_or_else(|| RpcError::params("unknown mode"))?;
                return self.set_mode(mode);
            }
            "model" | "safety_model" if !known(value) => {
                return Err(RpcError::params(format!("unknown model {value}")));
            }
            "effort"
                if value != "default"
                    && !self
                        .efforts(self.model().as_deref())
                        .iter()
                        .any(|e| e == value) =>
            {
                return Err(RpcError::params(format!("unknown effort {value}")));
            }
            "model" | "safety_model" | "effort" => {}
            _ => return Err(RpcError::params(format!("unknown config option {id}"))),
        }
        let effort = (value != "default").then(|| value.to_string());
        if !materialized {
            let mut st = self.state.lock().unwrap();
            match id {
                "model" => {
                    st.pending.model = Some(value.into());
                    st.pending.effort = Some(None);
                }
                "effort" => st.pending.effort = Some(effort),
                _ => st.pending.safety = Some(value.into()),
            }
            return Ok(());
        }
        match id {
            "model" => self.session.set_model(value),
            "effort" => self.session.set_effort(effort),
            _ => self.session.set_safety_model(value),
        }
        .map_err(|e| RpcError::params(e.message))
    }

    pub fn announce_settings(&self) {
        self.out.update(
            &self.acp_id,
            json!({"sessionUpdate": "current_mode_update", "currentModeId": mode_id(self.mode())}),
        );
        self.out.update(
            &self.acp_id,
            json!({"sessionUpdate": "config_option_update", "configOptions": self.config_options()}),
        );
    }

    fn message(&self, text: &str) {
        self.out.update(
            &self.acp_id,
            json!({"sessionUpdate": "agent_message_chunk", "content": text_block(text)}),
        );
    }

    /// Ask the user every pending question addressed to them (after load).
    pub fn restore_questions(self: &Arc<Self>) {
        self.plan(&View::default());
        for request in self.session.pending_scope_requests() {
            self.event(
                &mut View::default(),
                &self.session.cwd(),
                EngineEvent::ScopeRequestUpdate { request },
            );
        }
        for request in self.session.pending_reviewer_guidance() {
            self.event(
                &mut View::default(),
                &self.session.cwd(),
                EngineEvent::ReviewerGuidanceTrustRequest { request },
            );
        }
        for event in self.session.goal_requests() {
            match event {
                EngineEvent::GoalDraft { contract } => {
                    self.goal_elicit("draft", json!({"contract":contract}), None)
                }
                EngineEvent::RoadblockRequest {
                    agent_id,
                    roadblock,
                } => self.goal_elicit(
                    "roadblock",
                    json!({"agentId":agent_id,"roadblock":roadblock}),
                    Some(roadblock.id.clone()),
                ),
                EngineEvent::ProposalRequest { agent_id, proposal } => self.goal_elicit(
                    "proposal",
                    json!({"agentId":agent_id,"proposal":proposal}),
                    Some(proposal.id.clone()),
                ),
                _ => {}
            }
        }
        for q in self.session.pending_questions() {
            if q.recipient.is_none() {
                self.ask(q, None);
            }
        }
    }

    // ---- approvals and questions ------------------------------------------

    fn approve(self: &Arc<Self>, req: ApprovalRequest, call: Option<String>, cwd: PathBuf) {
        let content = mapping::approval_content(&req, &cwd);
        let synthetic = call.is_none();
        let call_id = call.unwrap_or_else(|| req.call_id.clone());
        if synthetic {
            self.out.update(
                &self.acp_id,
                json!({
                    "sessionUpdate": "tool_call", "toolCallId": call_id,
                    "title": format!("{} · {}", req.breadcrumb, mapping::tool_title(&req.tool, &req.requested, Some(&cwd))),
                    "kind": mapping::tool_kind(&req.tool), "status": "pending",
                    "rawInput": req.requested, "content": content,
                    "locations": mapping::locations(&req.tool, &req.requested, &cwd),
                }),
            );
        }
        let params = json!({
            "sessionId": self.acp_id,
            "toolCall": {"toolCallId": call_id, "status": "pending", "content": content,
                "_meta": {"cockpit-code": {"callId": req.call_id, "parentCallId": req.parent_call_id}}},
            "options": mapping::approval_options(&req),
        });
        conforms::<agent_client_protocol_schema::v1::RequestPermissionRequest>(
            "session/request_permission",
            &params,
        );
        let this = self.clone();
        tokio::spawn(async move {
            let decision = match this.out.request("session/request_permission", params).await {
                Ok(result) => mapping::approval_decision(&result),
                Err(e) => ApprovalDecision::Deny {
                    feedback: format!("user unavailable: {}", e.message),
                },
            };
            let allowed = !matches!(decision, ApprovalDecision::Deny { .. });
            if let Err(e) = this.session.respond_approval(&req.id, decision) {
                eprintln!("acp: approval {}: {}", req.id, e.message);
            }
            let status = match (allowed, synthetic) {
                (false, _) => "failed",
                (true, true) => "completed",
                (true, false) => "in_progress",
            };
            this.out.update(
                &this.acp_id,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": call_id, "status": status}),
            );
        });
    }

    fn ask(self: &Arc<Self>, q: QuestionRecord, call: Option<String>) {
        if !self.state.lock().unwrap().asked.insert(q.id.clone()) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let id = q.id.clone();
            this.ask_now(q, call).await;
            this.state.lock().unwrap().asked.remove(&id);
        });
    }

    async fn ask_now(&self, q: QuestionRecord, call: Option<String>) {
        let legacy = q.questions.is_empty();
        let specs = if legacy {
            vec![QuestionSpec {
                id: String::new(),
                text: q.text.clone(),
                kind: QuestionKind::Text,
                options: vec![],
                multi_select: false,
                allow_other: true,
            }]
        } else {
            q.questions.clone()
        };
        let mut header = format!("{} asks", q.breadcrumb);
        if let Some(note) = &q.note {
            header.push_str(&format!(" (escalated: {note})"));
        }
        let mut answers = None;
        if self.caps.elicitation_form {
            let mut params = json!({
                "mode": "form", "sessionId": self.acp_id,
                "message": format!("{header}:\n{}", specs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n")),
                "requestedSchema": mapping::elicitation_schema(&specs),
            });
            if let Some(call) = &call {
                params["toolCallId"] = json!(call);
            }
            conforms::<agent_client_protocol_schema::v1::CreateElicitationRequest>(
                "elicitation/create",
                &params,
            );
            match self.out.request("elicitation/create", params).await {
                Ok(r) => match r["action"].as_str() {
                    Some("accept") => {
                        answers = Some(mapping::elicitation_answers(&specs, &r["content"]))
                    }
                    Some("decline") => {
                        answers = Some(mapping::elicitation_answers(&specs, &json!({})))
                    }
                    // Cancelled: the question stays pending in the engine.
                    _ => return,
                },
                Err(e) if e.code == crate::rpc::METHOD_NOT_FOUND => {}
                Err(_) => return,
            }
        }
        let answers = match answers {
            Some(a) => a,
            None => {
                let mut out = vec![];
                for (i, spec) in specs.iter().enumerate() {
                    match self.ask_with_permission(&q, &header, i, spec).await {
                        Some(a) => out.push(a),
                        None => return,
                    }
                }
                out
            }
        };
        let result = if legacy {
            let a = &answers[0];
            self.session.answer(
                &q.id,
                if a.skipped {
                    "skipped".to_string()
                } else {
                    mapping::answer_summary(a)
                },
            )
        } else {
            self.session.answer_questions(&q.id, answers)
        };
        if let Err(e) = result {
            eprintln!("acp: question {}: {}", q.id, e.message);
        }
    }

    /// Fallback without form elicitation: a permission prompt per choice or
    /// boolean question; free text gets `user unavailable` plus a visible
    /// message so the user can reply in their next prompt.
    async fn ask_with_permission(
        &self,
        q: &QuestionRecord,
        header: &str,
        i: usize,
        spec: &QuestionSpec,
    ) -> Option<code_engine::schema::QuestionAnswer> {
        let Some(options) = mapping::question_options(spec) else {
            self.message(&format!(
                "\n\n> **{header}:** {}\n>\n> _This client cannot collect free-text answers, so the agent was told you are unavailable. Reply in your next message to answer._\n\n",
                spec.text.replace('\n', "\n> ")
            ));
            return Some(mapping::unavailable(spec));
        };
        let call_id = format!("question-{}-{i}", q.id);
        let mut body = spec.text.clone();
        if spec.multi_select {
            body.push_str("\n\n_(multiple choices allowed; this client can pick one)_");
        }
        self.out.update(
            &self.acp_id,
            json!({
                "sessionUpdate": "tool_call", "toolCallId": call_id,
                "title": format!("{header}: {}", clip(spec.text.lines().next().unwrap_or(""), 100)),
                "kind": "think", "status": "pending", "content": [text_content(&body)],
            }),
        );
        let params = json!({
            "sessionId": self.acp_id,
            "toolCall": {"toolCallId": call_id},
            "options": options,
        });
        let result = self
            .out
            .request("session/request_permission", params)
            .await
            .ok()?;
        let answer = mapping::question_option_answer(spec, &result);
        self.out.update(
            &self.acp_id,
            json!({
                "sessionUpdate": "tool_call_update", "toolCallId": call_id,
                "status": if answer.is_some() { "completed" } else { "failed" },
                "content": [text_content(&format!("{body}\n\n**Answer:** {}", answer.as_ref().map(mapping::answer_summary).unwrap_or_else(|| "cancelled".into())))],
            }),
        );
        answer
    }

    // ---- engine events -----------------------------------------------------

    async fn events(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<EngineEvent>) {
        let mut view = View::default();
        let cwd = self.session.cwd();
        let mut idle_timer = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            // Approval and review events are emitted directly while ToolStart
            // is relayed, so a root approval can arrive before its call: it
            // waits briefly for the call, then gets a call of its own.
            let deadline = view.deferred.first().map(|(_, at)| *at);
            let event = tokio::select! {
                e = rx.recv() => e,
                _ = idle_timer.tick() => {
                    let st = self.state.lock().unwrap();
                    let busy = st.inflight.is_some() || !st.queue.is_empty() || !st.engine_queued.is_empty();
                    if let Some(mut events) = self.session.idle_tick(busy) {
                        tokio::spawn(async move { while events.recv().await.is_some() {} });
                    }
                    continue;
                }
                _ = self.stop.notified() => break,
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                    for (request, _) in std::mem::take(&mut view.deferred) {
                        self.approve(request, None, cwd.clone());
                    }
                    continue;
                }
            };
            let Some(event) = event else { break };
            self.event(&mut view, &cwd, event);
        }
    }

    fn event(self: &Arc<Self>, v: &mut View, cwd: &Path, event: EngineEvent) {
        let sid = self.acp_id.as_str();
        let out = &self.out;
        match event {
            EngineEvent::ForkChat {
                fork_id,
                asker,
                kind,
                text,
                tool_call,
                tool_result,
                ..
            } => {
                let display = if kind == "assistant" {
                    format!("\nDiscussion ({}):\n{text}\n", asker)
                } else if kind == "error" {
                    format!("Discussion error: {text}\n")
                } else {
                    format!("[Discussion {kind}]\n")
                };
                out.update(sid,json!({"sessionUpdate":"agent_message_chunk","content":text_block(&display),"_meta":{"cockpit-code":{"forkChat":{"forkId":fork_id,"asker":asker,"kind":kind,"toolCall":tool_call,"toolResult":tool_result}}}}));
            }
            EngineEvent::DecisionUpdated { record, open_count } => {
                out.update(sid,json!({"sessionUpdate":"agent_message_chunk","content":text_block(&format!("Decision {}: {} → {} ({} open)\n",record.id,record.question,record.choice,open_count)),"_meta":{"cockpit-code":{"decision":record,"decisionOpenCount":open_count}}}));
            }
            EngineEvent::GuidanceSelected { selection } => {
                out.update(sid,json!({"sessionUpdate":"agent_message_chunk","content":text_block(&format!("Guidance {}: {}",selection.ids.join(", "),selection.reason)),"_meta":{"cockpit-code":{"guidanceSelection":selection}}}));
            }
            EngineEvent::SubscriptionNotice {
                agent_id,
                changes,
                overflow,
            } => {
                out.update(sid,json!({"sessionUpdate":"agent_message_chunk","content":text_block(&format!("Subscribed paths changed for {agent_id}: {}",changes.iter().map(|c|c.path.display().to_string()).collect::<Vec<_>>().join(", "))),"_meta":{"cockpit-code":{"subscriptionNotice":{"agentId":agent_id,"changes":changes,"overflow":overflow}}}}));
            }
            EngineEvent::ScopeRequestUpdate { request } => {
                out.update(sid,json!({"sessionUpdate":"tool_call","toolCallId":format!("scope-{}",request.id),"title":format!("Write scope: {} → {}",request.requester,request.owner),"kind":"other","status":match request.status.as_str() {"pending"|"applying"=>"pending","applied"|"granted"=>"completed",_=>"failed"},"rawInput":request,"_meta":{"cockpit-code":{"scopeRequest":true}}}));
                if request.status == "pending"
                    && let Some(why) = request.escalation.clone()
                    && self
                        .state
                        .lock()
                        .unwrap()
                        .asked
                        .insert(format!("scope-{}", request.id))
                {
                    let this = self.clone();
                    tokio::spawn(async move {
                        let params = json!({"sessionId":this.acp_id,"toolCall":{"toolCallId":format!("scope-{}",request.id),"title":format!("Scope decision: {}",request.path.display()),"status":"pending","rawInput":request,"content":[text_content(&format!("Owner {} asks you to decide: {why}",request.owner))]},"options":[{"optionId":"apply","name":"Apply change","kind":"allow_once"},{"optionId":"grant","name":"Grant changed directory","kind":"allow_always"},{"optionId":"reject","name":"Reject change","kind":"reject_once"}]});
                        if let Ok(result) =
                            this.out.request("session/request_permission", params).await
                        {
                            use code_engine::scopes::ScopeDecision;
                            let decision = match result["outcome"]["optionId"].as_str() {
                                Some("apply") => ScopeDecision::Apply,
                                Some("grant") => ScopeDecision::Grant { scope: vec![] },
                                Some("reject") => ScopeDecision::Reject {
                                    reason: "User rejected the escalated scope request".into(),
                                },
                                _ => return, // cancelled defers the request
                            };
                            if let Err(error) = this
                                .session
                                .resolve_scope_request(&request.id, decision)
                                .await
                            {
                                this.message(&error.message);
                            }
                        }
                    });
                }
            }
            EngineEvent::ReviewerGuidanceTrustRequest { request } => {
                let this = self.clone();
                tokio::spawn(async move {
                    let content = if request.tampered {
                        format!(
                            "An unapproved command changed reviewer guidance. The engine restored the approved state. {}\n\n{}",
                            if request.deleted {
                                "Approve to delete the guidance file."
                            } else {
                                "Approve to install this exact proposed content:"
                            },
                            request.content
                        )
                    } else {
                        request.content.clone()
                    };
                    let params = json!({"sessionId":this.acp_id,"toolCall":{"toolCallId":request.id,"title":format!("Trust {} guidance {}", if request.library {"project library"} else {"reviewer"}, request.path.display()),"_meta":{"cockpit-code":{"guidanceTrust":{"library":request.library,"path":request.path,"sha256":request.sha256,"tampered":request.tampered,"deleted":request.deleted}}},"status":"pending","content":[text_content(&content)]},"options":[{"optionId":"trust","name":if request.deleted {"Approve deletion"} else {"Trust this exact content"},"kind":"allow_once"},{"optionId":"ignore","name":"Keep approved state","kind":"reject_once"}]});
                    let result = this.out.request("session/request_permission", params).await;
                    let trust = result
                        .as_ref()
                        .is_ok_and(|r| r["outcome"]["optionId"] == "trust");
                    if let Err(error) = this.session.resolve_reviewer_guidance(&request.id, trust) {
                        this.message(&error.message);
                        let _ = this.session.resolve_reviewer_guidance(&request.id, false);
                    }
                });
            }
            EngineEvent::Pruned { report } => self.message(&format!(
                "\n_Saved about {} tokens ({} prompts, {} outputs)._\n",
                report.tokens_saved, report.delegations, report.tool_outputs
            )),
            EngineEvent::Reconnecting { .. } => self.message("\n_[reconnecting…]_\n"),
            EngineEvent::ModelSwitched { from, to, reason } => {
                self.message(&format!("\n_Model fallback: {from} → {to} ({reason})_\n"))
            }
            EngineEvent::RootCompactionSuggested { used, window, .. } => self.message(&format!(
                "Root context at {}% — consider /compact at a good stopping point.",
                used.saturating_mul(100) / window.max(1)
            )),
            EngineEvent::IdleCompacted => self.message("\n\n_[compacted while idle]_\n\n"),
            EngineEvent::DiscoveryUpdated { errors } => self.message(&if errors.is_empty() {
                "Discovery refreshed.".into()
            } else {
                format!("Discovery refreshed: {}", errors.join("; "))
            }),
            EngineEvent::GoalUpdate { state } => {
                let phase = (state.disposition.clone(), state.phase.clone());
                if v.goal_phase.as_ref() != Some(&phase) {
                    v.goal_phase = Some(phase);
                    self.plan(v);
                }
            }
            EngineEvent::GoalDraft { contract } => {
                self.goal_elicit("draft", json!({"contract":contract}), None)
            }
            EngineEvent::RoadblockRequest {
                agent_id,
                roadblock,
            } => self.goal_elicit(
                "roadblock",
                json!({"agentId":agent_id,"roadblock":roadblock}),
                Some(roadblock.id.clone()),
            ),
            EngineEvent::ProposalRequest { agent_id, proposal } => self.goal_elicit(
                "proposal",
                json!({"agentId":agent_id,"proposal":proposal}),
                Some(proposal.id.clone()),
            ),
            EngineEvent::Text { delta } => out.update(
                sid,
                json!({"sessionUpdate": "agent_message_chunk", "content": text_block(&delta)}),
            ),
            EngineEvent::Reasoning { delta } => out.update(
                sid,
                json!({"sessionUpdate": "agent_thought_chunk", "content": text_block(&delta)}),
            ),
            EngineEvent::ToolStart { call } => {
                if call.name == "todo" {
                    return;
                }
                let old = (call.name == "write")
                    .then(|| {
                        call.arguments["path"]
                            .as_str()
                            .map(|p| mapping::abs(cwd, p))
                    })
                    .flatten()
                    .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() < 256 * 1024))
                    .and_then(|p| std::fs::read_to_string(p).ok());
                let base = mapping::start_content(&call.name, &call.arguments, cwd, old);
                out.update(
                    sid,
                    json!({
                        "sessionUpdate": "tool_call", "toolCallId": call.id,
                        "title": mapping::tool_title(&call.name, &call.arguments, Some(cwd)),
                        "kind": mapping::tool_kind_with_args(&call.name,&call.arguments), "status": "in_progress",
                        "content": base, "rawInput": call.arguments,
                        "locations": mapping::locations(&call.name, &call.arguments, cwd),
                    }),
                );
                v.inflight.push(call.id.clone());
                let view = CallView {
                    base,
                    notes: v.early_notes.remove(&call.id).unwrap_or_default(),
                    output: None,
                };
                if !view.notes.is_empty() {
                    out.update(
                        sid,
                        json!({"sessionUpdate": "tool_call_update", "toolCallId": call.id, "content": view.content()}),
                    );
                }
                let (matched, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut v.deferred)
                    .into_iter()
                    .partition(|(r, _)| r.call_id == call.id);
                v.deferred = rest;
                v.calls.insert(call.id.clone(), view);
                for (request, _) in matched {
                    self.approve(request, Some(call.id.clone()), cwd.to_path_buf());
                }
            }
            EngineEvent::ToolEnd {
                id,
                name,
                output,
                is_error,
                details,
            } => {
                if name == "todo" {
                    return;
                }
                v.inflight.retain(|c| *c != id);
                let Some(call) = v.calls.get_mut(&id) else {
                    return;
                };
                call.output = mapping::end_text(&name, &output, is_error);
                let mut update = json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": id,
                    "status": if is_error { "failed" } else { "completed" },
                    "content": call.content(),
                    "rawOutput": {"output": clip(&output, 16 * 1024), "isError": is_error, "details": details},
                });
                if !details.edits.is_empty() {
                    update["locations"] = json!(mapping::edit_locations(&details));
                }
                if name == "task" {
                    // A finished child's report replaces its progress lines.
                    call.notes.clear();
                    update["content"] = json!(call.content());
                }
                out.update(sid, update);
            }
            EngineEvent::SafetyReview {
                call_id,
                model,
                phase,
                verdict,
                reason,
                ..
            } => {
                let note = if phase == "started" {
                    format!("Reviewing with {model}…")
                } else {
                    format!(
                        "Safety review ({model}): {} {}",
                        verdict.as_deref().unwrap_or("error"),
                        reason.as_deref().unwrap_or("")
                    )
                };
                let review = |notes: &mut Vec<String>| {
                    notes.retain(|n| !n.starts_with("Reviewing with "));
                    notes.push(note);
                };
                match v.calls.get_mut(&call_id) {
                    None => review(v.early_notes.entry(call_id).or_default()),
                    Some(call) => {
                        review(&mut call.notes);
                        out.update(
                        sid,
                        json!({"sessionUpdate": "tool_call_update", "toolCallId": call_id, "content": call.content()}),
                    );
                    }
                }
            }
            EngineEvent::ScriptCall {
                call,
                parent_call_id,
                job_id,
                result,
                agent_id,
                args_summary,
                ..
            } if agent_id == "root" => {
                if let Some(result) = result {
                    v.inflight.retain(|id| id != &call.id);
                    if let Some(view) = v.calls.get_mut(&call.id) {
                        view.output =
                            mapping::end_text(&call.name, &result.output, result.is_error);
                        out.update(
                            sid,
                            json!({"sessionUpdate":"tool_call_update", "toolCallId":call.id,
                            "status":if result.is_error {"failed"} else {"completed"},
                            "content":view.content(), "rawOutput":result,
                            "locations":mapping::edit_locations(&result.details)}),
                        );
                    }
                    if let Some(parent) = parent_call_id
                        && let Some(view) = v.calls.get_mut(&parent)
                    {
                        view.note(format!(
                            "{} {}({})",
                            if result.is_error { "✗" } else { "✓" },
                            call.name,
                            clip(&args_summary, 120)
                        ));
                        out.update(sid,json!({"sessionUpdate":"tool_call_update", "toolCallId":parent,"content":view.content()}));
                    }
                } else {
                    let base = mapping::start_content(&call.name, &call.arguments, cwd, None);
                    out.update(sid,json!({"sessionUpdate":"tool_call", "toolCallId":call.id,
                        "title":mapping::tool_title(&call.name,&call.arguments,Some(cwd)),
                        "kind":mapping::tool_kind_with_args(&call.name,&call.arguments),"status":"in_progress", "content":base,
                        "rawInput":call.arguments,"locations":mapping::locations(&call.name,&call.arguments,cwd),
                        "_meta":{"cockpit-code":{"parentCallId":parent_call_id,"jobId":job_id}}}));
                    v.inflight.push(call.id.clone());
                    v.calls.insert(
                        call.id,
                        CallView {
                            base,
                            notes: vec![],
                            output: None,
                        },
                    );
                }
            }
            EngineEvent::Todo { items } => {
                v.todos = items;
                self.plan(v);
            }
            EngineEvent::Committed { envelope } => match envelope.event {
                HistoryEvent::Goal { .. } if envelope.agent_id == "root" => self.plan(v),
                HistoryEvent::Compaction { summary } if envelope.agent_id == "root" => {
                    self.message(&format!("\n\n_Context compacted._\n\n{summary}\n\n"))
                }
                _ => {}
            },
            EngineEvent::Context { meter } => {
                if let Some(size) = meter.window {
                    let totals = self.session.totals();
                    let mut update =
                        json!({"sessionUpdate": "usage_update", "used": meter.used, "size": size});
                    if totals.known_cost_calls > 0 {
                        update["cost"] =
                            json!({"amount": totals.known_cost_usd, "currency": "USD"});
                    }
                    out.update(sid, update);
                }
            }
            EngineEvent::TitleChanged { title, .. } => out.update(
                sid,
                json!({"sessionUpdate": "session_info_update", "title": title}),
            ),
            EngineEvent::ApprovalRequest { request } => {
                let call = (request.agent_id == "root")
                    .then(|| request.call_id.clone())
                    .filter(|id| v.calls.contains_key(id));
                if call.is_none() && request.agent_id == "root" && request.parent_call_id.is_none()
                {
                    let at = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
                    v.deferred.push((request, at));
                } else {
                    self.approve(request, call, cwd.to_path_buf());
                }
            }
            EngineEvent::Question { question } => {
                if question.recipient.is_none() && question.answer.is_none() {
                    let call = (question.asker == "root")
                        .then(|| question.call_id.clone())
                        .flatten()
                        .filter(|c| v.calls.contains_key(c));
                    self.ask(question, call);
                }
            }
            EngineEvent::SessionClaimed { message, .. } => {
                self.message(&format!("\n\n⚠ {message}\n\n"))
            }
            EngineEvent::QueuedMessageDelivered { message } if message.agent == "root" => {
                self.delivered(message.id)
            }
            EngineEvent::PartialCompletion { native_reason, .. }
                if native_reason == "interrupted" =>
            {
                self.message("\n\n_[interrupted; the partial reply above is kept]_\n\n")
            }
            EngineEvent::MovedToBackground { reason, job_id, .. } => self.message(&format!(
                "\n\n_[moved to background ({reason}) as job {job_id}]_\n\n"
            )),
            EngineEvent::PartialCompletion { status, .. } => self.message(&format!(
                "\n\n_[incomplete reply discarded: {}]_\n\n",
                status.as_str()
            )),
            EngineEvent::StreamReset { .. } => {
                self.message("\n\n_[retrying; the partial reply above was discarded]_\n\n")
            }
            EngineEvent::Error { error } => {
                let mut st = self.state.lock().unwrap();
                if st.inflight.is_some() {
                    st.last_error = Some(error.message);
                } else {
                    drop(st);
                    self.message(&format!("\n\n⚠ {}\n\n", error.message));
                }
            }
            EngineEvent::TurnEnd { stop_reason } => self.turn_ended(&stop_reason),
            EngineEvent::AgentMessage { from, to, text, .. } if to == "root" && from != "jobs" => {
                if let Some(task) = v.child_task.get(&from).cloned() {
                    v.child_note(
                        out,
                        sid,
                        &task,
                        format!("{from} → root: {}", clip(&text, 200)),
                    );
                }
            }
            EngineEvent::Job { job } if job.owner == "root" && !job.running() => {
                self.message(&format!(
                    "\n\n_[background {} {} {}]_\n\n",
                    job.kind, job.id, job.status
                ))
            }
            other => v.child(out, sid, None, other),
        }
    }

    /// ACP plan entries mirror the root contract, including verification labels.
    fn plan(&self, _v: &View) {
        let contract = self.session.contract("root");
        let mut entries = Vec::new();
        let running = self
            .session
            .goal_status()
            .is_some_and(|g| g.disposition == "running" && g.phase != "drafting");
        let mut active = false;
        for (kind, list) in [
            "Acceptance",
            "Findings",
            "Rejection",
            "Non-goal",
            "Scope",
            "Step",
        ]
        .into_iter()
        .zip(contract.lists())
        {
            for c in list {
                let status = match c.status.as_str() {
                    "met" | "withdrawn" => "completed",
                    "hit" => "in_progress",
                    _ if running && !active && matches!(kind, "Acceptance" | "Findings") => {
                        active = true;
                        "in_progress"
                    }
                    _ => "pending",
                };
                entries.push(json!({"content":format!("{kind}: {} [{}; {}]",c.text,c.status,c.verification),"priority":if kind=="Step" {"medium"} else {"high"},"status":status}));
            }
        }
        self.out.update(
            &self.acp_id,
            json!({"sessionUpdate":"plan","entries":entries}),
        );
    }
    fn goal_elicit(self: &Arc<Self>, kind: &str, data: Value, record_id: Option<String>) {
        let kind = kind.to_owned();
        let options = match kind.as_str() {
            "draft" => {
                json!([{"optionId":"approve","name":"Approve this contract and run","kind":"allow_once"},{"optionId":"cancel","name":"Cancel draft","kind":"reject_once"}])
            }
            "roadblock" => {
                json!([{"optionId":"pivot","name":"Pivot: revisit the approach within the approved criteria","kind":"allow_once"},{"optionId":"stop","name":"Stop at this roadblock","kind":"reject_once"},{"optionId":"override","name":"Override: accept this recorded roadblock as a known risk","kind":"allow_once"}])
            }
            _ => {
                json!([{"optionId":"accept","name":"Accept the proposed criterion change","kind":"allow_once"},{"optionId":"reject","name":"Reject; keep the approved criterion","kind":"reject_once"}])
            }
        };
        let params = json!({"sessionId":self.acp_id,"toolCall":{"toolCallId":format!("goal-{}",record_id.as_deref().unwrap_or("draft")),"title":format!("Goal {kind}"),"status":"pending","content":[text_content(&serde_json::to_string_pretty(&data).unwrap())]},"options":options});
        conforms::<agent_client_protocol_schema::v1::RequestPermissionRequest>(
            "session/request_permission",
            &params,
        );
        let this = self.clone();
        tokio::spawn(async move {
            let result = this.out.request("session/request_permission", params).await;
            let selected = result
                .as_ref()
                .ok()
                .and_then(|r| r["outcome"]["optionId"].as_str())
                .unwrap_or(if kind == "draft" {
                    "cancel"
                } else if kind == "roadblock" {
                    "stop"
                } else {
                    "reject"
                })
                .to_owned();
            // GoalDraft/RoadblockRequest precede TurnEnd; resolution starts only once idle.
            if kind != "proposal" {
                while this.session.is_active() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
            let result = match kind.as_str() {
                "draft" if selected == "approve" => this
                    .session
                    .goal_approve(serde_json::from_value(data["contract"].clone()).unwrap())
                    .map(Some),
                "draft" => this.session.goal_cancel_draft().map(|()| None),
                "roadblock" => {
                    let note=match selected.as_str() {"override"=>Some(format!("Owner selected ACP override: accept roadblock {} as a known risk.",record_id.as_deref().unwrap_or(""))),"pivot"=>Some("Owner selected ACP pivot: revisit the approach within the approved criteria; ask for specific amendments if required.".into()),_=>None};
                    this.session.resolve_roadblock(
                        record_id.as_deref().unwrap(),
                        &selected,
                        note,
                        None,
                    )
                }
                _ => this
                    .session
                    .resolve_proposal(
                        data["agentId"].as_str().unwrap(),
                        record_id.as_deref().unwrap(),
                        &selected,
                        None,
                    )
                    .map(|()| None),
            };
            match result {
                Ok(Some(mut rx)) => while rx.recv().await.is_some() {},
                Ok(None) => {}
                Err(e) => this.message(&format!("Goal decision failed: {}", e.message)),
            }
        });
    }
}

struct CallView {
    base: Vec<Value>,
    notes: Vec<String>,
    output: Option<String>,
}

impl CallView {
    fn note(&mut self, line: String) {
        self.notes.push(line);
        if self.notes.len() > 12 {
            self.notes.remove(0);
        }
    }
    fn content(&self) -> Vec<Value> {
        let mut c = self.base.clone();
        if !self.notes.is_empty() {
            c.push(text_content(&self.notes.join("\n")));
        }
        if let Some(o) = &self.output {
            c.push(text_content(o));
        }
        c
    }
}

#[derive(Default)]
struct View {
    goal_phase: Option<(String, String)>,
    calls: HashMap<String, CallView>,
    /// Root calls started and not yet ended, oldest first.
    inflight: Vec<String>,
    /// Every subagent to the root `task` call that (transitively) spawned it.
    child_task: HashMap<String, String>,
    status: HashMap<String, String>,
    todos: Value,
    /// Review notes for calls whose ToolStart has not arrived yet.
    early_notes: HashMap<String, Vec<String>>,
    /// Root approvals waiting for their ToolStart, with a deadline.
    deferred: Vec<(ApprovalRequest, tokio::time::Instant)>,
}

impl View {
    fn child_note(&mut self, out: &Out, sid: &str, task: &str, line: String) {
        if let Some(call) = self.calls.get_mut(task) {
            call.note(line);
            out.update(
                sid,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": task, "content": call.content()}),
            );
        }
    }

    /// Subagent activity, summarised under its root `task` call: start,
    /// status changes, tool titles, failures and the final report. Child
    /// text and reasoning streams are not forwarded.
    fn child(&mut self, out: &Out, sid: &str, agent: Option<(&str, &str)>, event: EngineEvent) {
        let task_of = |v: &Self, id: &str| v.child_task.get(id).cloned();
        match event {
            EngineEvent::AgentStarted {
                agent: record,
                execution,
                breadcrumb,
                task_call_id,
            } => {
                let parent = record.parent_id.clone().unwrap_or_else(|| "root".into());
                let task = if parent == "root" {
                    task_call_id
                } else {
                    task_of(self, &parent)
                };
                let Some(task) = task else { return };
                self.child_task
                    .insert(record.agent_id.clone(), task.clone());
                let line = format!(
                    "▸ {breadcrumb} started ({}{}){}",
                    record.model.as_deref().unwrap_or("default model"),
                    if execution.background {
                        ", background"
                    } else {
                        ""
                    },
                    if parent == "root" {
                        String::new()
                    } else {
                        format!(
                            ": {}",
                            clip(execution.task.lines().next().unwrap_or(""), 80)
                        )
                    }
                );
                self.child_note(out, sid, &task, line);
            }
            EngineEvent::AgentStatus {
                agent_id,
                status,
                breadcrumb,
                ..
            } => {
                if agent_id == "root" || self.status.get(&agent_id) == Some(&status) {
                    return;
                }
                self.status.insert(agent_id.clone(), status.clone());
                if let Some(task) = task_of(self, &agent_id) {
                    let shown = match status.strip_prefix("queued:") {
                        Some(n) => format!("queued ({n})"),
                        None => status,
                    };
                    self.child_note(out, sid, &task, format!("{breadcrumb}: {shown}"));
                }
            }
            EngineEvent::AgentFinished {
                agent_id,
                status,
                report,
                breadcrumb,
                ..
            } => {
                if let Some(task) = task_of(self, &agent_id) {
                    let line = format!(
                        "■ {breadcrumb} {status}: {}",
                        clip(report.trim().lines().next().unwrap_or(""), 200)
                    );
                    self.child_note(out, sid, &task, line);
                }
            }
            EngineEvent::AgentEvent {
                agent_id,
                breadcrumb,
                event,
            } => self.child(out, sid, Some((&agent_id, &breadcrumb)), *event),
            EngineEvent::Context { meter } if agent.is_some() => {
                let (id, breadcrumb) = agent.unwrap();
                if let Some(task) = task_of(self, id) {
                    // A child's context belongs to its task, never to the
                    // session-wide usage_update meter owned by the root.
                    out.update(
                        sid,
                        json!({
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": task,
                            "_meta": {"cockpit-code": {"contextMeter": {
                                "agentId": id, "breadcrumb": breadcrumb, "meter": meter
                            }}}
                        }),
                    );
                }
            }
            EngineEvent::ToolStart { call } if agent.is_some() => {
                let (id, crumb) = agent.unwrap();
                if call.name == "todo" {
                    return;
                }
                if let Some(task) = task_of(self, id) {
                    let line = format!(
                        "{crumb} › {}",
                        mapping::tool_title(&call.name, &call.arguments, None)
                    );
                    self.child_note(out, sid, &task, line);
                }
            }
            EngineEvent::ToolEnd {
                name,
                is_error: true,
                output,
                ..
            } if agent.is_some() => {
                let (id, crumb) = agent.unwrap();
                if let Some(task) = task_of(self, id) {
                    let line = format!(
                        "{crumb} ✗ {name}: {}",
                        clip(output.lines().next().unwrap_or(""), 120)
                    );
                    self.child_note(out, sid, &task, line);
                }
            }
            EngineEvent::SafetyReview {
                phase,
                verdict,
                model,
                ..
            } if agent.is_some() && phase != "started" => {
                let (id, crumb) = agent.unwrap();
                if let Some(task) = task_of(self, id) {
                    let line = format!(
                        "{crumb} safety review ({model}): {}",
                        verdict.as_deref().unwrap_or("error")
                    );
                    self.child_note(out, sid, &task, line);
                }
            }
            EngineEvent::Error { error } if agent.is_some() => {
                let (id, crumb) = agent.unwrap();
                if let Some(task) = task_of(self, id) {
                    self.child_note(
                        out,
                        sid,
                        &task,
                        format!("{crumb} error: {}", clip(&error.message, 200)),
                    );
                }
            }
            _ => {}
        }
    }
}
