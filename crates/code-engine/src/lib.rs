//! In-process coding-agent API: sessions, persistence, events and frontend channels.
pub mod attachments;
pub mod bundled_skills;
mod compaction;
pub mod context;
mod discovery;
mod exclusive;
mod file_notices;
mod flat;
pub use code_config::SubagentMode;
mod finalize;
mod frontend;
mod progress;
pub use exclusive::{Exclusive, ExclusiveSafety};
mod guidance;
pub mod guidance_library;
pub mod guidance_selector;
pub mod hidden_fork;
mod idle;
pub use guidance::ReviewerGuidanceRequest;
mod agents;
mod decisions;
pub use decisions::{DecisionRecord, DecisionReview};
mod ids;
mod subtree;
pub use subtree::ModelPolicy;
mod injection;
mod intel;
pub mod jobs;
mod local_command;
mod loop_guard;
pub mod lsp_defaults;
mod mail;
pub mod mcp;
pub mod model;
mod outputs;
mod pages;
pub mod paths;
pub mod permissions;
mod pressure;
mod prune;
mod pty;
pub use prune::PruneReport;
mod prior_thinking;
mod question_chat;
mod questions;
mod queue;
pub use queue::QueuedMessage;
pub mod goal;
pub use goal::{Criterion, CriterionCheck, Finding, GoalContract, Proposal, Roadblock};
mod goal_controller;
pub use goal_controller::{GoalLifecycle, GoalRun};
mod foreground;
mod goal_review;
pub use question_chat::{ForkChatMessage, QuestionFork};
mod read_only;
mod repair;
mod reporting;
mod retry;
mod reviewer;
mod scheduler;
pub mod schema;
pub mod scopes;
pub mod script;
mod session_tools;
pub mod sessions;
mod skill_commands;
pub mod subscriptions;
pub mod terminals;
mod text_recovery;
pub mod think;
pub mod tool_defaults;
pub mod tool_names;
mod tool_profiles;
pub mod tools;
mod tree;
pub mod usage;
mod user_terminal;
mod variations;
pub mod wire_schema;
use code_config::{ConfigStore, PermissionMode, Snapshot};
use futures_util::{StreamExt, stream};
use model::EventSender;
use schema::*;
use sessions::{Journal, SessionSettings};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tools::{CommitBarrier, PermissionGate, ToolBox, ToolOutput};
use uuid::Uuid;
pub const BUILD_PROMPT: &str = include_str!("builtin/build.md");
pub const PLAN_PROMPT: &str = include_str!("builtin/plan.md");
pub const EXPLORE_PROMPT: &str = include_str!("builtin/explore.md");
#[derive(Clone, Default)]
pub struct SessionOptions {
    pub model: Option<String>,
    pub effort: Option<String>,
}
pub(crate) struct Control {
    data_dir: PathBuf,
    directories: code_config::Directories,
    guidance_selection_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    guidance_turns: Mutex<HashMap<String, Arc<Mutex<guidance_selector::Turn>>>>,
    guidance: Mutex<guidance::Trust>,
    subscription_watch: Mutex<subscriptions::WatchState>,
    scope_serviced: Mutex<std::collections::BTreeSet<String>>,
    scope_applies: Mutex<HashMap<String, (Arc<CommitBarrier>, CancellationToken)>>,
    idle: Mutex<idle::IdleState>,
    compact_suggestion: Mutex<Option<(String, bool)>>,
    config: Arc<Mutex<Snapshot>>,
    pub journal: Mutex<Journal>,
    usage: Arc<usage::Cache>,
    pub approvals: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    claimed_emitted: AtomicBool,
    front: Mutex<Option<EventSender>>,
    subscribers: Mutex<Vec<EventSender>>,
    meter: Mutex<ContextMeter>,
    runtime: tree::Runtime,
    terminals: terminals::Hub,
    script_values: Mutex<HashMap<String, std::collections::BTreeMap<String, serde_json::Value>>>,
    changed: tokio::sync::Notify,
    mcp: Arc<mcp::Manager>,
    lsp: Arc<tools::lsp::Manager>,
    last_root_request: Mutex<Option<(model::Model, model::PreparedRequest)>>,
    title_work: Mutex<TitleWork>,
    scheduler: Arc<scheduler::Scheduler>,
    health_cancel: CancellationToken,
    fork_busy: Mutex<std::collections::BTreeSet<String>>,
    exclusive_policy: Arc<Mutex<Option<Exclusive>>>,
    queue: Mutex<queue::Queue>,
    mail_since: Mutex<HashMap<String, tokio::time::Instant>>,
    goal_clock: Mutex<goal_controller::GoalClock>,
    goal_progress: AtomicU64,
    discovery_pending: Mutex<std::collections::BTreeSet<String>>,
}
#[derive(Default)]
struct TitleWork {
    cancel: CancellationToken,
    closing: bool,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Control {
    async fn auxiliary(
        &self,
        model: &model::Model,
        request: &model::PreparedRequest,
        cancel: &CancellationToken,
    ) -> Result<model::Completion, EngineError> {
        let owner = scopes::execution_owner(&request.agent_id);
        let display = if owner == request.agent_id {
            request.agent_id.clone()
        } else {
            format!("scope-aux:{}", request.agent_id)
        };
        let _slot = self
            .scheduler
            .short_named(&owner, &display, model, cancel)
            .await?;
        self.check_owner()?;
        self.auxiliary_unscheduled(model, request, cancel).await
    }
    async fn auxiliary_unscheduled(
        &self,
        model: &model::Model,
        request: &model::PreparedRequest,
        cancel: &CancellationToken,
    ) -> Result<model::Completion, EngineError> {
        self.check_owner()?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = {
            let pending = model.stream(request, &tx, cancel);
            tokio::pin!(pending);
            loop {
                tokio::select! {
                    result = &mut pending => break result,
                    Some(event) = rx.recv() => {
                        if let EngineEvent::Usage { usage } = event {
                            self.commit(&request.agent_id, HistoryEvent::Usage { usage: usage.clone() })?;
                            self.emit(EngineEvent::Usage { usage });
                        }
                    }
                }
            }
        };
        drop(tx);
        while let Some(event) = rx.recv().await {
            if let EngineEvent::Usage { usage } = event {
                self.commit(
                    &request.agent_id,
                    HistoryEvent::Usage {
                        usage: usage.clone(),
                    },
                )?;
                self.emit(EngineEvent::Usage { usage });
            }
        }
        result
    }
    fn change_title(
        &self,
        title: String,
        source: TitleSource,
        generation: Option<u64>,
    ) -> Result<bool, EngineError> {
        let envelope = self
            .journal
            .lock()
            .unwrap()
            .set_title(title.clone(), source, generation)?;
        if let Some(envelope) = envelope {
            self.emit(EngineEvent::Committed { envelope });
            self.emit(EngineEvent::TitleChanged { title, source });
            return Ok(true);
        }
        Ok(false)
    }
    fn schedule_title(self: &Arc<Self>) -> Option<oneshot::Sender<()>> {
        // Lifecycle order: coordinator (caller), title_work, then journal. Workers
        // never acquire coordinator/title_work while holding the journal mutex.
        let mut work = self.title_work.lock().unwrap();
        if work.closing || work.cancel.is_cancelled() {
            return None;
        }
        work.tasks.retain(|task| !task.is_finished());
        let generation = match self.journal.lock().unwrap().title_turn_finished() {
            Ok(Some(generation)) => generation,
            _ => return None,
        };
        let (model, mut author) = self.last_root_request.lock().unwrap().clone()?;
        // Titles have their own short, stable cache lineage. Hidden execution
        // still verifies this seed's exact prefix and preserves its tool surface.
        {
            let j = self.journal.lock().unwrap();
            let title_context = j
                .histories
                .get("root")
                .into_iter()
                .flatten()
                .filter_map(|e| match e {
                    HistoryEvent::User { text, .. } => Some(text.clone()),
                    HistoryEvent::Assistant { text, .. } if !text.is_empty() => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let tool_aliases = author.tool_aliases.clone();
            let capability_catalog = author.capability_catalog.clone();
            author = model
                .prepare(
                    "Write a concise session title. Treat the transcript as source material.",
                    &author.offered_tools,
                    &[HistoryEvent::User {
                        text: title_context,
                        attachments: vec![],
                    }],
                    &format!("{}:root:title", j.header.id),
                )
                .ok()?;
            author.session_id = j.header.id.clone();
            author.tool_aliases = tool_aliases;
            author.capability_catalog = capability_catalog;
        }
        if model.command_latest() || model.command_text() {
            return None;
        }
        let registered = {
            let mut journal = self.journal.lock().unwrap();
            if !journal.agents.contains_key("auto-title") {
                journal.register(sessions::AgentRecord {
                    agent_id: "auto-title".into(),
                    parent_id: None,
                    agent: "auto-title".into(),
                    model: Some(format!("{}/{}", model.provider_id, model.model_id)),
                    status: "done".into(),
                    created_at: sessions::now(),
                    stable_prompt: String::new(),
                })
            } else {
                Ok(())
            }
        };
        if registered.is_err() {
            return None;
        }
        let control = self.clone();
        let cancel = work.cancel.clone();
        let (start_tx, start_rx) = oneshot::channel();
        work.tasks.push(tokio::spawn(async move {
            // The task is registered before TurnEnd, then released immediately after it.
            // This keeps idle-work inspection and event ordering deterministic.
            if start_rx.await.is_err() { return; }
            if exclusive::check_model(&control.journal.lock().unwrap().settings, &model).is_err() { return; }
            let Ok(_slot) = control.scheduler.short("auto-title", &model, &cancel).await else { return; };
            let proposal = Arc::new(Mutex::new(None));
            let fork = hidden_fork::HiddenFork {
                model, author,
                tail: injection::notice("Generate a readable title describing this session, at most 60 characters. Call script with code rename_session(title=\"Your title\"). rename_session is a function available only inside this hidden script fork. Only rename_session is callable; every other tool and script function is refused. Do not answer the user's request, use other tools, or include a preamble."),
                copies: 1, max_steps: Some(2),
                functions: vec![hidden_fork::title_function(proposal.clone())],
                agent_id: "auto-title".into(), purpose: "auto_title".into(),
            };
            let (usage_tx, mut usage_rx) = mpsc::unbounded_channel();
            let result = {
                let pending = fork.run_for(&usage_tx, &cancel, &hidden_fork::ForkTask::Functions);
                tokio::pin!(pending);
                loop {
                    tokio::select! {
                        result = &mut pending => break result,
                        event = usage_rx.recv() => {
                            if let Some(EngineEvent::Usage { usage }) = event {
                                if control.commit("auto-title", HistoryEvent::Usage { usage: usage.clone() }).is_err() {
                                    cancel.cancel();
                                }
                                control.emit(EngineEvent::Usage { usage });
                            }
                        }
                    }
                }
            };
            drop(usage_tx);
            while let Some(EngineEvent::Usage { usage }) = usage_rx.recv().await {
                let _ = control.commit("auto-title", HistoryEvent::Usage { usage: usage.clone() });
                control.emit(EngineEvent::Usage { usage });
            }
            let outcomes = result.unwrap_or_else(|_| vec![HiddenForkOutcome { accepted_plain_text: false,
                purpose: "auto_title".into(), copy: 0, steps: 0, succeeded: false,
                reason: Some("hidden fork setup failed".into()),
                author_prefix_bytes: 0, author_prefix_sha256: String::new(), cache_hit_observed: false,
            }]);
            if !cancel.is_cancelled() && outcomes.iter().any(|outcome| outcome.succeeded) {
                let title = proposal.lock().unwrap().take();
                if let Some(title) = title { let _ = control.change_title(title, TitleSource::Auto, Some(generation)); }
            }
            for outcome in outcomes {
                let _ = control.commit("auto-title", HistoryEvent::HiddenForkOutcome { outcome: outcome.clone() });
                control.emit(EngineEvent::HiddenForkFinished { outcome });
            }
            let _ = control.agent_status("auto-title", "done");
            control.changed.notify_waiters();
        }));
        Some(start_tx)
    }
    fn emit(&self, mut event: EngineEvent) {
        match &mut event {
            EngineEvent::Error { error } => {
                error.message = self.tool_names("root").rewrite(&error.message)
            }
            EngineEvent::AgentEvent {
                agent_id, event, ..
            } => {
                if let EngineEvent::Error { error } = event.as_mut() {
                    error.message = self.tool_names(agent_id).rewrite(&error.message);
                }
            }
            _ => {}
        }
        let failure = match &event {
            EngineEvent::Error { error } => Some(("root", error)),
            EngineEvent::AgentEvent {
                agent_id, event, ..
            } => match event.as_ref() {
                EngineEvent::Error { error } => Some((agent_id.as_str(), error)),
                _ => None,
            },
            _ => None,
        };
        if let Some((agent, error)) = failure {
            // Persistence failure must not suppress delivery of the original failure.
            let _ = self.commit(
                agent,
                HistoryEvent::TurnError {
                    error: error.clone(),
                },
            );
        }

        if let Some(tx) = &*self.front.lock().unwrap() {
            let _ = tx.send(event.clone());
        }
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| tx.send(event.clone()).is_ok());
    }
    fn claimed(&self, error: &EngineError) {
        if error.kind == ErrorKind::ReadOnly && !self.claimed_emitted.swap(true, Ordering::AcqRel) {
            self.emit(EngineEvent::SessionClaimed {
                session_id: self.journal.lock().unwrap().header.id.clone(),
                message: error.message.clone(),
            });
        }
    }
    fn check_owner(&self) -> Result<(), EngineError> {
        let result = self.journal.lock().unwrap().check_owner();
        if let Err(e) = &result {
            self.claimed(e);
        }
        result
    }
    fn commit(&self, agent: &str, mut event: HistoryEvent) -> Result<EventEnvelope, EngineError> {
        if let HistoryEvent::SystemReminder { text } = &mut event {
            // Source-bearing notices stay byte-faithful; rewrite pure harness prose only.
            let source = [
                "Message from ",
                "Answer to question ",
                "Latest unanswered",
                "Model reminder:",
                "Your current contract",
                "Authoritative current criteria:",
            ]
            .iter()
            .any(|prefix| text.starts_with(prefix));
            if !source && !text.contains(['\n', '{', '[']) {
                *text = self.tool_names(agent).rewrite(text);
            }
        }
        if let HistoryEvent::User { text, .. } = &mut event {
            text.push_str(&self.context_tag(agent, text));
        }
        let result = self.journal.lock().unwrap().append(agent, event);
        match result {
            Ok(envelope) => {
                if let HistoryEvent::Usage { usage } = &envelope.event {
                    self.usage.observe(&envelope.ts, usage);
                    if let Some(run) = self.goal_state().filter(|g| g.disposition == "running") {
                        self.save_goal(run)?;
                    }
                }
                self.emit(EngineEvent::Committed {
                    envelope: envelope.clone(),
                });
                if agent == "root"
                    && matches!(envelope.event, HistoryEvent::Goal { .. })
                    && let Some(run) = self.goal_state()
                {
                    self.emit(EngineEvent::GoalUpdate {
                        state: Box::new(run),
                    });
                }

                if let HistoryEvent::DecisionRecorded { record }
                | HistoryEvent::DecisionReviewed { record } = &envelope.event
                {
                    let open_count = self
                        .journal
                        .lock()
                        .unwrap()
                        .decisions
                        .values()
                        .filter(|r| r.is_open())
                        .count();
                    self.emit(EngineEvent::DecisionUpdated {
                        record: Box::new(record.clone()),
                        open_count,
                    });
                }
                if let Some(record) = decisions::automatic(&envelope) {
                    self.record_decision(record)?;
                }
                Ok(envelope)
            }
            Err(e) => {
                self.claimed(&e);
                Err(e)
            }
        }
    }
    fn ask_question(
        &self,
        asker: &str,
        text: String,
        blocking: bool,
    ) -> Result<String, EngineError> {
        let questions = questions::parse(&serde_json::json!({"text":text}))?;
        self.ask_questions(asker, None, questions, blocking)
    }
    fn ask_questions(
        &self,
        asker: &str,
        call_id: Option<String>,
        questions: Vec<QuestionSpec>,
        blocking: bool,
    ) -> Result<String, EngineError> {
        let question = {
            let j = self.journal.lock().unwrap();
            let agent = j
                .agents
                .get(asker)
                .ok_or_else(|| EngineError::bad("asker is not registered"))?;
            QuestionRecord {
                discussing: false,
                call_id,
                text: questions
                    .iter()
                    .map(|q| q.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                questions,
                answers: None,
                blocking,
                id: Uuid::new_v4().to_string(),
                asker: asker.into(),
                recipient: if j.settings.subagents == SubagentMode::Flat {
                    None
                } else {
                    agent.parent_id.clone()
                },
                breadcrumb: session_tools::breadcrumb(&j.agents, asker),
                answer: None,
                note: None,
            }
        };
        self.commit(
            asker,
            HistoryEvent::Question {
                question: question.clone(),
            },
        )?;
        if let Some(parent) = &question.recipient {
            self.message(
                asker,
                parent,
                format!(
                    "<question id=\"{}\" from=\"{}\">{}</question>",
                    question.id,
                    asker,
                    serde_json::to_string(&question.questions).unwrap()
                ),
            )?;
        }
        self.changed.notify_waiters();
        self.emit(EngineEvent::Question {
            question: question.clone(),
        });
        Ok(question.id)
    }
    fn setting(&self, key: &str, value: serde_json::Value) -> Result<(), EngineError> {
        self.settings_batch(vec![(key.into(), value)])
    }
    fn settings_batch(&self, changes: Vec<(String, serde_json::Value)>) -> Result<(), EngineError> {
        let result = self.journal.lock().unwrap().change_settings(changes);
        match result {
            Ok(envelopes) => {
                *self.exclusive_policy.lock().unwrap() =
                    self.journal.lock().unwrap().settings.exclusive.clone();
                for envelope in envelopes {
                    self.emit(EngineEvent::Committed { envelope });
                }
                Ok(())
            }
            Err(e) => {
                self.claimed(&e);
                Err(e)
            }
        }
    }
}
struct State {
    store: ConfigStore,
    tools: ToolBox,
    gate: Arc<dyn PermissionGate>,
    config: Arc<Mutex<Snapshot>>,
    control: Arc<Control>,
    cwd: PathBuf,
    anchor: Option<(u64, u64)>,
    agent_id: String,
    remaining_depth: Option<usize>,
    model_override: Option<String>,
    effort_override: Option<String>,
    compact_request: Arc<Mutex<Option<Option<String>>>>,
    job_id: Option<String>,
    script_depth: usize,
    parent_call_id: Option<String>,
    warned_context: bool,
    compacted_estimate: Option<u64>,
    child_profile: Option<model::ChildRequest>,
    continuing: bool,
    guidance_turn: Arc<Mutex<guidance_selector::Turn>>,
    guidance_notices: Arc<Mutex<Vec<String>>>,
    context_state: context::BoundaryState,
    active_model: Option<model::Model>,
}
struct ActiveTurn {
    idle_compaction: bool,
    id: String,
    cancel: CancellationToken,
    barrier: Arc<CommitBarrier>,
}
#[derive(Default)]
struct Coordinator {
    active: Option<ActiveTurn>,
    closing: bool,
    mail_suspended: bool,
}
#[derive(Clone)]
pub struct Session {
    directories: code_config::Directories,
    state: Arc<AsyncMutex<State>>,
    control: Arc<Control>,
    coordinator: Arc<Mutex<Coordinator>>,
    config: Arc<Mutex<Snapshot>>,
    /// The session policy (shared grants), for work outside the root state lock (D64 fork chats).
    gate: Arc<dyn PermissionGate>,
    _lifetime: Arc<SessionLifetime>,
}
struct SessionLifetime {
    control: Arc<Control>,
    coordinator: Arc<Mutex<Coordinator>>,
}
impl Drop for SessionLifetime {
    fn drop(&mut self) {
        if self.control.journal.lock().unwrap().is_closed() {
            return;
        }
        let idle = self.coordinator.lock().unwrap().active.is_none()
            && self.control.terminals.user.lock().unwrap().is_empty()
            && !self.control.runtime.has_unfinished()
            && !self.control.mcp.has_connections()
            && !self.control.lsp.has_connections()
            && !self
                .control
                .title_work
                .lock()
                .unwrap()
                .tasks
                .iter()
                .any(|task| !task.is_finished());
        if idle {
            self.control.script_values.lock().unwrap().clear();
            self.control.health_cancel.cancel();
            let _ = self.control.journal.lock().unwrap().close();
            return;
        }
        self.coordinator.lock().unwrap().closing = true;
        // A dropped future must never release the lock before its workers stop.
        self.control.journal.lock().unwrap().release_on_drop = false;
        self.control.cancel_tree(&self.coordinator);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let control = self.control.clone();
            let coordinator = self.coordinator.clone();
            runtime.spawn(async move {
                let _ = shutdown_tree(control, coordinator).await;
            });
        }
        // Outside a runtime cancellation is best effort; leave a crash-style stale lock.
    }
}
impl Session {
    pub fn create(
        store: ConfigStore,
        cwd: PathBuf,
        options: SessionOptions,
    ) -> Result<Self, EngineError> {
        let cwd = canonical_cwd(cwd)?;
        let settings = SessionSettings {
            model: options.model,
            effort: options.effort,
            safety_model: store.snapshot().config.permissions.auto_model.clone(),
            subagents: store.snapshot().config.subagents.mode,
            ..Default::default()
        };
        let journal = Journal::create(&store, cwd, settings)?;
        Self::from_journal(store, journal, None)
    }
    /// Compatibility constructor. A custom gate deliberately overrides session policy.
    pub fn new(
        store: ConfigStore,
        cwd: PathBuf,
        options: SessionOptions,
        gate: Arc<dyn PermissionGate>,
    ) -> Result<Self, EngineError> {
        let cwd = canonical_cwd(cwd)?;
        let settings = SessionSettings {
            model: options.model,
            effort: options.effort,
            safety_model: store.snapshot().config.permissions.auto_model.clone(),
            subagents: store.snapshot().config.subagents.mode,
            ..Default::default()
        };
        let journal = Journal::create(&store, cwd, settings)?;
        Self::from_journal(store, journal, Some(gate))
    }
    pub fn resume(
        store: ConfigStore,
        id: &str,
        options: SessionOptions,
    ) -> Result<Self, EngineError> {
        let journal = Journal::load(&store, id, false)?;
        let session = Self::from_journal(store, journal, None)?;
        session.apply_options(options)?;
        Ok(session)
    }
    pub fn claim(
        store: ConfigStore,
        id: &str,
        options: SessionOptions,
    ) -> Result<Self, EngineError> {
        let journal = Journal::load(&store, id, true)?;
        let session = Self::from_journal(store, journal, None)?;
        session.apply_options(options)?;
        Ok(session)
    }
    pub fn fork(
        store: ConfigStore,
        id: &str,
        point: Option<sessions::ForkPoint>,
        options: SessionOptions,
    ) -> Result<Self, EngineError> {
        let journal = Journal::fork(&store, id, point)?;
        let session = Self::from_journal(store, journal, None)?;
        session.apply_options(options)?;
        Ok(session)
    }
    pub fn restore(
        store: ConfigStore,
        snapshot: sessions::SessionSnapshot,
    ) -> Result<Self, EngineError> {
        let journal = Journal::restore(&store, snapshot, None)?;
        Self::from_journal(store, journal, None)
    }
    fn from_journal(
        store: ConfigStore,
        mut journal: Journal,
        custom_gate: Option<Arc<dyn PermissionGate>>,
    ) -> Result<Self, EngineError> {
        let cwd = canonical_cwd(journal.header.cwd.clone())?;
        freeze_strict_tools(&mut journal, store.snapshot())?;
        if journal.settings.lsp_servers.is_none() {
            let resolved = lsp_defaults::resolve(&store.snapshot().config.lsp, &cwd).servers;
            if journal.is_persisted() && !journal.read_only {
                journal
                    .change_settings(vec![("lspServers".into(), serde_json::json!(resolved))])?;
            } else {
                journal.settings.lsp_servers = Some(resolved);
            }
        }
        let lsp_servers = journal.settings.lsp_servers.clone().unwrap_or_default();
        // Older sessions acquire their initial default once; descendants always read
        // this single durable field rather than their independently reloaded config.
        if journal.settings.exclusive.is_none()
            && journal.settings.safety_model.is_none()
            && let Some(id) = &store.snapshot().config.permissions.auto_model
        {
            permissions::validate_model(store.snapshot(), id)?;
            if !journal.read_only {
                journal.change_settings(vec![("safetyModel".into(), serde_json::json!(id))])?;
            } else {
                journal.settings.safety_model = Some(id.clone());
            }
        }
        if let Some(id) = &exclusive::safety(&journal.settings) {
            permissions::validate_model(store.snapshot(), id)?;
        }
        let config = Arc::new(Mutex::new(store.snapshot().clone()));
        let enabled = journal.settings.mcps.clone().unwrap_or_else(|| {
            store
                .snapshot()
                .mcps
                .iter()
                .filter(|(_, m)| m.enabled_by_default)
                .map(|(n, _)| n.clone())
                .collect()
        });
        journal.settings.mcps = Some(enabled.clone());
        let effective_enabled = exclusive::enabled_mcps(&journal.settings, store.snapshot());
        let mcp = Arc::new(mcp::Manager::new(
            store.snapshot(),
            &effective_enabled,
            cwd.clone(),
        ));
        let exclusive_policy = Arc::new(Mutex::new(journal.settings.exclusive.clone()));
        exclusive::register_policy(journal.header.id.clone(), &exclusive_policy);
        let usage = usage::Cache::start(&store);
        let control = Arc::new(Control {
            data_dir: store.directories.data.clone(),
            directories: store.directories.clone(),
            guidance_turns: Mutex::default(),
            guidance_selection_locks: Mutex::default(),
            guidance: Mutex::default(),
            subscription_watch: Mutex::default(),
            scope_serviced: Mutex::default(),
            scope_applies: Mutex::default(),
            idle: Mutex::default(),
            compact_suggestion: Mutex::default(),
            config: config.clone(),
            usage,
            exclusive_policy,
            journal: Mutex::new(journal),
            approvals: Mutex::default(),
            claimed_emitted: AtomicBool::new(false),
            front: Mutex::new(None),
            subscribers: Mutex::default(),
            meter: Mutex::default(),
            runtime: tree::Runtime::default(),
            terminals: terminals::Hub::default(),
            script_values: Mutex::default(),
            changed: tokio::sync::Notify::new(),
            mcp,
            lsp: Arc::new(tools::lsp::Manager::new(config.clone(), lsp_servers)),
            last_root_request: Mutex::default(),
            title_work: Mutex::default(),
            scheduler: Arc::default(),
            health_cancel: CancellationToken::new(),
            fork_busy: Mutex::default(),
            queue: Mutex::default(),
            mail_since: Mutex::default(),
            goal_clock: Mutex::default(),
            goal_progress: AtomicU64::new(0),
            discovery_pending: Mutex::default(),
        });
        let weak = Arc::downgrade(&control);
        control.scheduler.set_status(Arc::new(move |id, status| {
            if let Some(control) = weak.upgrade() {
                if id.starts_with("goal-review-")
                    || id.starts_with("guidance:")
                    || id.starts_with("scope-aux:")
                {
                    return;
                }
                if id.starts_with("question-fork-") {
                    control.fork_status(id, status);
                } else {
                    let _ = control.agent_status(id, status);
                }
            }
        }));
        control.watch_reviewer_guidance(&cwd)?;
        control.restore_guidance_requests()?;
        control.restore_interrupted()?;
        control.restore_scopes()?;
        control.start_subscription_watch()?;
        let gate = permissions::session_gate(control.clone(), config.clone(), custom_gate);
        let mut tools = ToolBox::new(cwd.clone(), gate.clone());
        tools.lsp = control.lsp.clone();
        tools.control = Arc::downgrade(&control);
        let anchor = {
            let j = control.journal.lock().unwrap();
            j.histories
                .get("root")
                .filter(|_| j.requests.contains_key("root"))
                .and_then(|h| {
                    h.iter().rev().find_map(|e| {
                        if let HistoryEvent::Usage { usage } = e {
                            if usage.kind == "agent" && !usage.partial {
                                usage.tokens.input_tokens.zip(usage.input_estimate)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                })
        };
        let state = State {
            store,
            tools,
            gate: gate.clone(),
            config: config.clone(),
            control: control.clone(),
            cwd,
            anchor,
            agent_id: "root".into(),
            remaining_depth: config.lock().unwrap().config.subagents.max_depth,
            model_override: None,
            effort_override: None,
            compact_request: Arc::new(Mutex::new(None)),
            job_id: None,
            script_depth: 0,
            parent_call_id: None,
            warned_context: false,
            compacted_estimate: None,
            child_profile: None,
            continuing: false,
            guidance_turn: control.guidance_turn("root"),
            guidance_notices: Arc::default(),
            context_state: context::BoundaryState::default(),
            active_model: None,
        };
        let coordinator = Arc::new(Mutex::new(Coordinator {
            mail_suspended: control.journal.lock().unwrap().root_mail_suspended,
            ..Coordinator::default()
        }));
        let lifetime = Arc::new(SessionLifetime {
            control: control.clone(),
            coordinator: Arc::clone(&coordinator),
        });
        let session = Self {
            directories: state.store.directories.clone(),
            _lifetime: lifetime,
            state: Arc::new(AsyncMutex::new(state)),
            control,
            coordinator,
            config,
            gate,
        };
        session.watch_mail_turns();
        session.refresh_discovery();
        Ok(session)
    }
    fn apply_options(&self, options: SessionOptions) -> Result<(), EngineError> {
        if let Some(model) = options.model {
            self.set_model(model)?;
        }
        if let Some(effort) = options.effort {
            self.set_effort(Some(effort))?;
        }
        Ok(())
    }
    pub fn id(&self) -> String {
        self.control.journal.lock().unwrap().header.id.clone()
    }
    /// Set a user-owned title. Pending automatic passes can no longer replace it.
    pub fn rename(&self, title: impl Into<String>) -> Result<(), EngineError> {
        self.control
            .change_title(title.into().trim().into(), TitleSource::User, None)?;
        Ok(())
    }
    pub fn title(&self) -> String {
        self.control.journal.lock().unwrap().header.title.clone()
    }
    pub fn title_generation_running(&self) -> bool {
        self.control
            .title_work
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|task| !task.is_finished())
    }
    pub fn cwd(&self) -> PathBuf {
        self.control.journal.lock().unwrap().header.cwd.clone()
    }
    pub fn totals(&self) -> SessionTotals {
        self.control.journal.lock().unwrap().totals.clone()
    }
    pub fn settings(&self) -> SessionSettings {
        self.control.journal.lock().unwrap().settings.clone()
    }
    pub fn agents(&self) -> Vec<sessions::AgentRecord> {
        self.control
            .journal
            .lock()
            .unwrap()
            .agents
            .values()
            .filter(|a| !a.hidden())
            .cloned()
            .collect()
    }
    pub fn jobs(&self) -> Vec<JobRecord> {
        let ids = self
            .control
            .journal
            .lock()
            .unwrap()
            .jobs
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        ids.iter()
            .filter_map(|id| self.control.job_record(id).ok())
            .collect()
    }
    pub fn agent_executions(&self) -> Vec<AgentExecution> {
        self.control
            .journal
            .lock()
            .unwrap()
            .executions
            .values()
            .cloned()
            .collect()
    }
    pub fn agent_totals(&self, id: &str) -> SessionTotals {
        let id = self.resolve_agent_id(id).unwrap_or_else(|_| id.into());
        self.control
            .journal
            .lock()
            .unwrap()
            .agent_totals
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }
    pub fn pending_questions(&self) -> Vec<QuestionRecord> {
        self.control
            .journal
            .lock()
            .unwrap()
            .questions
            .values()
            .filter(|q| q.answer.is_none())
            .cloned()
            .collect()
    }
    pub fn context_meter(&self) -> ContextMeter {
        self.control.meter.lock().unwrap().clone()
    }
    pub fn context_window(&self) -> Option<u64> {
        let settings = self.settings();
        let snapshot = self.config.lock().unwrap();
        let id = settings
            .model
            .as_deref()
            .or(snapshot.config.default_model.as_deref())?;
        let (p, m) = id.split_once('/')?;
        snapshot.providers.get(p)?.models.get(m)?.context_window
    }
    pub fn lock_status(&self) -> Result<sessions::LockStatus, EngineError> {
        self.control.journal.lock().unwrap().lock_status()
    }
    pub fn snapshot(&self) -> Result<sessions::SessionSnapshot, EngineError> {
        self.control.journal.lock().unwrap().snapshot()
    }
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<EngineEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.control.subscribers.lock().unwrap().push(tx);
        rx
    }
    pub fn set_model(&self, id: impl Into<String>) -> Result<(), EngineError> {
        if self.exclusive().is_some() {
            return Err(EngineError::bad(
                "model is locked by exclusive mode; turn exclusive off first",
            ));
        }
        let id = id.into();
        let snapshot = self.config.lock().unwrap().clone();
        let settings = self.settings();
        // Synchronous constructors/settings may select a model while its
        // provider is loading. Every inference boundary awaits readiness.
        if !discovery::pending_model(&snapshot, &id) {
            build_model(
                &snapshot,
                &SessionSettings {
                    model: Some(id.clone()),
                    effort: None,
                    ..settings
                },
            )?;
        }
        self.control.settings_batch(vec![
            ("model".into(), serde_json::json!(id)),
            ("effort".into(), serde_json::Value::Null),
        ])
    }
    pub fn set_effort(&self, effort: Option<String>) -> Result<(), EngineError> {
        let snapshot = self.config.lock().unwrap().clone();
        let mut settings = self.settings();
        settings.effort = effort.clone();
        let id = settings
            .model
            .as_ref()
            .or(snapshot.config.default_model.as_ref());
        if !id.is_some_and(|id| discovery::pending_model(&snapshot, id)) {
            build_model(&snapshot, &settings)?;
        }
        self.control.setting("effort", serde_json::json!(effort))
    }
    /// Select the safety reviewer for every agent and script in this session.
    /// This host API is intentionally absent from model-facing tools.
    pub fn set_safety_model(&self, id: impl Into<String>) -> Result<(), EngineError> {
        if self.exclusive().is_some() {
            return Err(EngineError::bad(
                "safety model is controlled by exclusive mode",
            ));
        }
        let id = id.into();
        permissions::validate_model(&self.config.lock().unwrap(), &id)?;
        self.control.setting("safetyModel", serde_json::json!(id))
    }
    pub fn safety_model(&self) -> Option<String> {
        exclusive::safety(&self.settings())
    }
    pub fn set_mode(&self, mode: PermissionMode) -> Result<(), EngineError> {
        if mode == PermissionMode::Unattended && self.control.has_frozen_terminal() {
            return Err(EngineError::bad(
                "this session froze a user terminal tool; unattended mode requires a new session",
            ));
        }
        exclusive::check_mode(&self.settings(), mode)?;
        self.control.setting("mode", serde_json::json!(mode))
    }
    pub fn set_mcps(&self, mcps: std::collections::BTreeSet<String>) -> Result<(), EngineError> {
        if self.coordinator.lock().unwrap().closing {
            return Err(EngineError::bad("session is shutting down"));
        }
        let snapshot = self.config.lock().unwrap();
        if mcps.iter().any(|id| !snapshot.mcps.contains_key(id)) {
            return Err(EngineError::bad("MCP is not configured"));
        }
        drop(snapshot);
        self.control.setting("mcps", serde_json::json!(mcps))?;
        self.control.mcp.set_enabled(&exclusive::enabled_mcps(
            &self.settings(),
            &self.config.lock().unwrap(),
        ));
        let agents = self.agents();
        for agent in agents.into_iter().filter(|a| !a.hidden()) {
            let text = format!(
                "MCP enabled servers changed to {}. MCP tools are available only through script as mcp.<server>.<tool>; use tools.search(query) or tools.describe(name).",
                serde_json::json!(mcps)
            );
            self.control.commit(
                &agent.agent_id,
                HistoryEvent::Message {
                    from: "mcp".into(),
                    to: agent.agent_id.clone(),
                    text,
                },
            )?;
        }
        self.control.changed.notify_waiters();
        Ok(())
    }
    pub fn mcp_status(&self) -> Vec<mcp::McpStatus> {
        self.control.mcp.statuses()
    }
    pub fn set_mcp_enabled(&self, name: &str, enabled: bool) -> Result<(), EngineError> {
        let mut names = self.settings().mcps.unwrap_or_default();
        if !self.config.lock().unwrap().mcps.contains_key(name) {
            return Err(EngineError::bad("MCP is not configured"));
        }
        if enabled {
            names.insert(name.into());
        } else {
            names.remove(name);
        }
        self.set_mcps(names)
    }
    pub fn set_subagent_toggles(
        &self,
        models: std::collections::BTreeSet<String>,
        providers: std::collections::BTreeSet<String>,
    ) -> Result<(), EngineError> {
        self.control.settings_batch(vec![
            ("disabledModels".into(), serde_json::json!(models)),
            ("disabledProviders".into(), serde_json::json!(providers)),
        ])
    }
    pub fn set_compact_at(&self, model: String, value: f64) -> Result<(), EngineError> {
        if !(0.0..=1.0).contains(&value) {
            return Err(EngineError::bad("compactAt must be 0..1"));
        }
        let mut overrides = self.settings().compact_at;
        overrides.insert(model, value);
        self.control
            .setting("compactAt", serde_json::json!(overrides))
    }
    /// Registry seam for the subagent lane. Register a parent before any child.
    pub fn register_agent(&self, mut agent: sessions::AgentRecord) -> Result<(), EngineError> {
        agent.parent_id = agent
            .parent_id
            .as_deref()
            .map(|id| self.resolve_agent_id(id))
            .transpose()?;
        self.control
            .journal
            .lock()
            .unwrap()
            .register(agent.clone())?;
        self.control
            .commit(&agent.agent_id.clone(), HistoryEvent::AgentState { agent })?;
        Ok(())
    }
    pub fn update_agent(&self, mut agent: sessions::AgentRecord) -> Result<(), EngineError> {
        agent.agent_id = self.resolve_agent_id(&agent.agent_id)?;
        agent.parent_id = agent
            .parent_id
            .as_deref()
            .map(|id| self.resolve_agent_id(id))
            .transpose()?;
        self.control
            .journal
            .lock()
            .unwrap()
            .update_agent(agent.clone())?;
        self.control
            .commit(&agent.agent_id.clone(), HistoryEvent::AgentState { agent })?;
        Ok(())
    }
    pub fn set_role(&self, role: String, stable_prompt: String) -> Result<(), EngineError> {
        let lifecycle = self.coordinator.lock().unwrap();
        if lifecycle.active.is_some() {
            return Err(EngineError::bad("change role only while idle"));
        }
        let mut j = self.control.journal.lock().unwrap();
        if j.histories.get("root").is_some_and(|h| {
            h.iter()
                .any(|e| matches!(e, HistoryEvent::PromptContext { .. }))
        }) {
            return Err(EngineError::bad(
                "the session prompt is already frozen; start a new session to change its role",
            ));
        }
        let mut agent = j.agents["root"].clone();
        agent.agent = role;
        agent.stable_prompt = stable_prompt;
        j.update_agent(agent.clone())?;
        drop(j);
        self.control
            .commit("root", HistoryEvent::AgentState { agent })?;
        Ok(())
    }
    pub fn respond_approval(
        &self,
        id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), EngineError> {
        self.control
            .approvals
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| EngineError::bad("approval is not pending"))?
            .send(decision)
            .map_err(|_| EngineError::bad("approval caller ended"))
    }
    pub fn ask_question(
        &self,
        asker: &str,
        text: impl Into<String>,
    ) -> Result<String, EngineError> {
        self.control
            .ask_question(&self.resolve_agent_id(asker)?, text.into(), false)
    }
    pub fn answer(&self, id: &str, text: impl Into<String>) -> Result<(), EngineError> {
        let text = text.into();
        let q = self
            .control
            .journal
            .lock()
            .unwrap()
            .questions
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::bad("question not found"))?;
        if q.questions.is_empty() {
            self.control.answer_question(id, text)?;
            Ok(())
        } else {
            self.answer_questions(id, questions::text_answers(&q, &text))
        }
    }
    /// Submit an entire D62 set by tool call ID (or legacy question ID).
    pub fn answer_questions(
        &self,
        call_id: &str,
        answers: Vec<QuestionAnswer>,
    ) -> Result<(), EngineError> {
        self.control.answer_set(call_id, answers)?;
        Ok(())
    }
    fn wake_answer_turn(&self) {
        // The mailbox watcher coalesces answers with other child mail and
        // respects user cancellation and dialogs, including fork discussions.
        self.control.changed.notify_waiters();
    }
    pub fn escalate(&self, id: &str, note: Option<String>) -> Result<(), EngineError> {
        self.control.escalate_question(id, note)
    }
    pub fn enqueue_message(
        &self,
        from: String,
        to: String,
        text: String,
    ) -> Result<(), EngineError> {
        let from = self.resolve_agent_id(&from).unwrap_or(from);
        self.control
            .message(&from, &self.resolve_agent_id(&to)?, text)
    }
    pub fn question_answer(&self, id: &str) -> Option<String> {
        self.control
            .journal
            .lock()
            .unwrap()
            .questions
            .get(id)
            .and_then(|q| q.answer.clone())
    }
    pub fn cancel(&self) {
        let _ = self.control.pause_goal("user");
        let mut coordinator = self.coordinator.lock().unwrap();
        coordinator.mail_suspended = true;
        let _ = self.control.commit("root", HistoryEvent::RootMailSuspended);
        if let Some(turn) = &coordinator.active {
            turn.cancel.cancel();
            turn.barrier.revoke();
        }
        drop(coordinator);
        self.control.changed.notify_waiters();
    }
    pub fn is_active(&self) -> bool {
        self.coordinator.lock().unwrap().active.is_some()
    }
    pub fn running_work(&self) -> RunningWork {
        let j = self.control.journal.lock().unwrap();
        RunningWork {
            jobs: j.jobs.values().filter(|j| j.running()).cloned().collect(),
            children: j
                .agents
                .values()
                .filter(|a| {
                    a.agent_id != "root"
                        && !a.hidden()
                        && (matches!(a.status.as_str(), "running" | "waiting" | "compacting")
                            || a.status.starts_with("queued"))
                })
                .cloned()
                .collect(),
        }
    }
    /// Cancel and join the whole tree, persist final states, then release the lock.
    /// All Session clones become closed. This remains safe if its caller is cancelled.
    pub async fn shutdown(&self) -> Result<(), EngineError> {
        self.coordinator.lock().unwrap().closing = true;
        let control = self.control.clone();
        let coordinator = self.coordinator.clone();
        tokio::spawn(shutdown_tree(control, coordinator))
            .await
            .map_err(|e| EngineError::bad(e.to_string()))?
    }
    pub fn close(&self) -> Result<(), EngineError> {
        let coordinator = self.coordinator.lock().unwrap();
        if coordinator.active.is_some() {
            return Err(EngineError::bad("cancel and await TurnEnd before closing"));
        }
        let work = self.running_work();
        if !self.control.terminals.user.lock().unwrap().is_empty()
            || !work.jobs.is_empty()
            || !work.children.is_empty()
            || self.control.runtime.has_unfinished()
            || self.control.mcp.has_connections()
            || self.control.lsp.has_connections()
            || self.title_generation_running()
        {
            return Err(EngineError::bad(
                "running work requires Session::shutdown().await",
            ));
        }
        self.control.journal.lock().unwrap().close()?;
        self.control.script_values.lock().unwrap().clear();
        Ok(())
    }
    pub fn send(&self, prompt: impl Into<String>) -> mpsc::UnboundedReceiver<EngineEvent> {
        self.send_with_attachments(prompt, vec![])
    }
    pub fn send_with_attachments(
        &self,
        prompt: impl Into<String>,
        attachments: Vec<Attachment>,
    ) -> mpsc::UnboundedReceiver<EngineEvent> {
        let prompt = prompt.into();
        if (self.control.discussing("root") && self.is_active())
            || self
                .coordinator
                .lock()
                .unwrap()
                .active
                .as_ref()
                .is_some_and(|t| t.idle_compaction)
        {
            let (tx, rx) = mpsc::unbounded_channel();
            let session = self.clone();
            tokio::spawn(async move {
                loop {
                    let changed = session.control.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if !session.is_active() {
                        break;
                    }
                    changed.await;
                }
                session.resume_discussion("root");
                let mut events = session.launch(Action::Send(prompt, attachments));
                while let Some(event) = events.recv().await {
                    if tx.send(event).is_err() {
                        session.cancel();
                        break;
                    }
                }
            });
            return rx;
        }
        self.resume_discussion("root");
        self.launch(Action::Send(prompt, attachments))
    }
    pub fn compact(&self, focus: Option<String>) -> mpsc::UnboundedReceiver<EngineEvent> {
        self.launch(Action::Compact(focus))
    }
    fn launch(&self, action: Action) -> mpsc::UnboundedReceiver<EngineEvent> {
        let (front_tx, front_rx) = mpsc::unbounded_channel();
        let mut coordinator = self.coordinator.lock().unwrap();
        if matches!(&action, Action::Mail)
            && (coordinator.closing
                || coordinator.active.is_some()
                || coordinator.mail_suspended
                || self.control.journal.lock().unwrap().root_mail_suspended
                || self.mail_dialog_open()
                || !self.control.root_mail_pending())
        {
            return front_rx;
        }
        if coordinator.closing || coordinator.active.is_some() {
            let error = EngineError::bad("session is closing or a turn is already active");
            let _ = self.control.commit(
                "root",
                HistoryEvent::TurnError {
                    error: error.clone(),
                },
            );
            let _ = front_tx.send(EngineEvent::Error { error });
            let _ = front_tx.send(EngineEvent::TurnEnd {
                stop_reason: "error".into(),
            });
            return front_rx;
        }
        if matches!(
            &action,
            Action::Send(..) | Action::Queued | Action::GoalDraft(..)
        ) {
            coordinator.mail_suspended = false;
        }
        let cancel = CancellationToken::new();
        let barrier = Arc::new(CommitBarrier::default());
        let turn_id = Uuid::new_v4().to_string();
        let idle_compaction = matches!(&action, Action::IdleCompact);
        if matches!(
            &action,
            Action::Send(..)
                | Action::Queued
                | Action::Mail
                | Action::GoalDraft(..)
                | Action::GoalContinue
        ) {
            *self.control.idle.lock().unwrap() = idle::IdleState::default();
        }
        coordinator.active = Some(ActiveTurn {
            idle_compaction,
            id: turn_id.clone(),
            cancel: cancel.clone(),
            barrier: barrier.clone(),
        });
        *self.control.front.lock().unwrap() = Some(front_tx);
        self.control.journal.lock().unwrap().turn_id = Some(turn_id.clone());
        self.control.journal.lock().unwrap().attempt_id = None;
        self.control
            .journal
            .lock()
            .unwrap()
            .activities
            .insert("root".into(), (turn_id.clone(), None));
        drop(coordinator);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = self.control.clone();
        let relay_cancel = cancel.clone();
        let relay = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if control
                    .front
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|tx| tx.is_closed())
                {
                    relay_cancel.cancel();
                }
                control.emit(event);
            }
        });
        let state = self.state.clone();
        let answer_waker = self.clone();
        let control = self.control.clone();
        let coordinator = self.coordinator.clone();
        let is_send = matches!(&action, Action::Send(prompt, attachments) if !prompt.trim().is_empty() || !attachments.is_empty())
            || matches!(&action, Action::Queued);
        tokio::spawn(async move {
            let result = {
                let mut state = state.lock().await;
                state.active_model = None;
                match action {
                    Action::Send(prompt, attachments) => {
                        match answer_waker.import_attachment_tags(prompt, attachments) {
                            Ok((prompt, attachments)) => {
                                let action = Action::Send(prompt, attachments);
                                goal_controller::drive(&mut state, action, &tx, &cancel, barrier)
                                    .await
                            }
                            Err(error) => Err(error),
                        }
                    }
                    action => {
                        goal_controller::drive(&mut state, action, &tx, &cancel, barrier).await
                    }
                }
            };
            drop(tx);
            let _ = relay.await;
            let title_eligible = is_send
                && result.as_ref().is_ok_and(|reason| reason != "discuss")
                && !cancel.is_cancelled();
            let stop_reason = match result {
                Ok(reason) => reason,
                Err(error) => {
                    control.claimed(&error);
                    control.emit(EngineEvent::Error { error });
                    if cancel.is_cancelled() {
                        "cancelled".into()
                    } else {
                        "error".into()
                    }
                }
            };
            // TurnEnd is emitted under the lifecycle lock after every mutation is joined.
            let mut coordinator = coordinator.lock().unwrap();
            if coordinator.active.as_ref().is_some_and(|t| t.id == turn_id) {
                coordinator.active = None;
            }
            control.journal.lock().unwrap().turn_id = None;
            control.journal.lock().unwrap().attempt_id = None;
            control.journal.lock().unwrap().activities.remove("root");
            // A cancel can race the initial User append. Reassert its durable
            // state after the turn has joined, so that append cannot clear it.
            if coordinator.mail_suspended && cancel.is_cancelled() {
                let _ = control.commit("root", HistoryEvent::RootMailSuspended);
            }
            let _ = control.agent_status("root", "waiting");
            let title_start = if title_eligible && !coordinator.closing {
                control.schedule_title()
            } else {
                None
            };
            control.emit(EngineEvent::TurnEnd { stop_reason });
            *control.front.lock().unwrap() = None;
            if let Some(start) = title_start {
                let _ = start.send(());
            }
            control.changed.notify_waiters();
            drop(coordinator);
            if !cancel.is_cancelled() {
                answer_waker.wake_queue_turn();
            }
        });
        front_rx
    }
}
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RunningWork {
    pub jobs: Vec<JobRecord>,
    pub children: Vec<sessions::AgentRecord>,
}
async fn shutdown_tree(
    control: Arc<Control>,
    coordinator: Arc<Mutex<Coordinator>>,
) -> Result<(), EngineError> {
    control.health_cancel.cancel();
    control.cancel_tree(&coordinator);
    loop {
        let notified = control.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if coordinator.lock().unwrap().active.is_none() {
            break;
        }
        notified.await;
    }
    control.finish_owned("root").await;
    control.abort_user_terminals().await;
    let terminal_workers = std::mem::take(&mut control.terminals.work.lock().unwrap().tasks);
    for worker in terminal_workers {
        let _ = worker.await;
    }
    let titles = {
        let mut work = control.title_work.lock().unwrap();
        work.closing = true;
        work.cancel.cancel();
        std::mem::take(&mut work.tasks)
    };
    for task in titles {
        let _ = task.await;
    }
    // Discussion continuations may have registered a child while cancellation began.
    control.finish_owned("root").await;
    control.mcp.shutdown().await;
    control.lsp.shutdown().await;
    control.script_values.lock().unwrap().clear();
    control.journal.lock().unwrap().close()
}
impl Control {
    fn cancel_tree(&self, coordinator: &Mutex<Coordinator>) {
        if let Some(turn) = &coordinator.lock().unwrap().active {
            turn.barrier.revoke();
            turn.cancel.cancel();
        }
        self.runtime.cancel_all();
        {
            let mut work = self.terminals.work.lock().unwrap();
            work.closing = true;
            work.cancel.cancel();
        }
        self.title_work.lock().unwrap().cancel.cancel();
    }
}
enum Action {
    GoalDraft(String),
    GoalContinue,
    Queued,
    Send(String, Vec<Attachment>),
    Mail,
    Compact(Option<String>),
    Prune,
    IdleCompact,
}
fn canonical_cwd(cwd: PathBuf) -> Result<PathBuf, EngineError> {
    let resolver =
        paths::Resolver::new(std::env::current_dir().map_err(|e| EngineError::bad(e.to_string()))?);
    let cwd = resolver
        .resolve(
            cwd.to_str()
                .ok_or_else(|| EngineError::bad("cwd is not UTF-8"))?,
        )
        .map_err(|e| EngineError::bad(e.to_string()))?;
    if !cwd.is_dir() {
        return Err(EngineError::bad("cwd is not a directory"));
    }
    Ok(cwd)
}
fn freeze_strict_tools(journal: &mut Journal, snapshot: &Snapshot) -> Result<(), EngineError> {
    let mut strict = journal.settings.strict_tools.clone();
    for (id, provider) in &snapshot.providers {
        for (model, config) in &provider.models {
            strict
                .entry(format!("{id}/{model}"))
                .or_insert_with(|| provider.strict_tools(config));
        }
    }
    if strict != journal.settings.strict_tools {
        if journal.is_persisted() && !journal.read_only {
            journal.change_settings(vec![("strictTools".into(), serde_json::json!(strict))])?;
        } else {
            journal.settings.strict_tools = strict;
        }
    }
    Ok(())
}
fn build_model(
    snapshot: &Snapshot,
    settings: &SessionSettings,
) -> Result<model::Model, EngineError> {
    let id = settings
        .exclusive
        .as_ref()
        .map(|ex| ex.model.as_str())
        .or(settings.model.as_deref())
        .or(snapshot.config.default_model.as_deref())
        .ok_or_else(|| EngineError::bad("set defaultModel or pass --model provider/model"))?;
    let (p, m) = id
        .split_once('/')
        .ok_or_else(|| EngineError::bad("model must be provider/model"))?;
    let provider = snapshot
        .providers
        .get(p)
        .ok_or_else(|| EngineError::bad(format!("provider {p} is not configured")))?;
    let config = provider
        .models
        .get(m)
        .ok_or_else(|| EngineError::bad(format!("model {id} is not configured")))?;
    let mut config = config.clone();
    config.strict_tools = settings
        .strict_tools
        .get(id)
        .copied()
        .or(config.strict_tools);
    model::Model::new(
        p.into(),
        m.into(),
        provider.clone(),
        config,
        settings.effort.as_deref(),
    )
}
impl State {
    fn reload(&mut self) -> Result<(), EngineError> {
        match self.store.reload_if_changed() {
            Ok(_) => self.context_state.reload_error = None,
            Err(error) => {
                let frozen = self.context_state.frozen.is_some()
                    || self
                        .history()
                        .iter()
                        .any(|event| matches!(event, HistoryEvent::PromptContext { .. }));
                if !frozen {
                    return Err(EngineError::bad(error.to_string()));
                }
                let message = error.to_string();
                if self.context_state.reload_error.as_ref() != Some(&message) {
                    self.control.commit(&self.agent_id, HistoryEvent::Message { from:"config".into(), to:self.agent_id.clone(), text:format!("Configuration reload failed; retaining the last valid configuration and the frozen session prompt: {message}") })?;
                    self.context_state.reload_error = Some(message);
                }
            }
        }
        *self.config.lock().unwrap() = self.store.snapshot().clone();
        self.control.usage.update_config(self.store.snapshot());
        freeze_strict_tools(
            &mut self.control.journal.lock().unwrap(),
            self.store.snapshot(),
        )?;
        let enabled = exclusive::enabled_mcps(
            &self.control.journal.lock().unwrap().settings,
            self.store.snapshot(),
        );
        self.control.mcp.refresh(self.store.snapshot(), &enabled);
        Ok(())
    }
    fn model(&self) -> Result<model::Model, EngineError> {
        if let Some(model) = &self.active_model {
            return Ok(model.clone());
        }
        if self.agent_id == "root"
            && let Some(goal) = self
                .control
                .goal_state()
                .filter(|g| g.disposition == "running")
        {
            let (provider, model) = goal
                .model
                .split_once('/')
                .ok_or_else(|| EngineError::bad("invalid frozen goal model"))?;
            let mut provider_config = self
                .store
                .snapshot()
                .providers
                .get(provider)
                .ok_or_else(|| EngineError::bad("frozen goal provider removed"))?
                .clone();
            provider_config.prompt_caching = goal.caching.clone();
            return model::Model::new(
                provider.into(),
                model.into(),
                provider_config,
                goal.model_config,
                goal.effort.as_deref(),
            );
        }
        if self
            .control
            .journal
            .lock()
            .unwrap()
            .settings
            .exclusive
            .is_some()
        {
            return build_model(self.store.snapshot(), &self.model_settings());
        }
        if let Some(profile) = &self.child_profile {
            let id = self
                .model_override
                .as_deref()
                .ok_or_else(|| EngineError::bad("child model missing"))?;
            let (p, m) = id
                .split_once('/')
                .ok_or_else(|| EngineError::bad("invalid child model"))?;
            let mut provider = self
                .store
                .snapshot()
                .providers
                .get(p)
                .ok_or_else(|| EngineError::bad("child provider removed"))?
                .clone();
            provider.prompt_caching = profile
                .primary_caching
                .clone()
                .unwrap_or_else(|| profile.caching.clone());
            let mut config = profile
                .primary_config
                .clone()
                .unwrap_or_else(|| profile.config.clone());
            config.strict_tools = self
                .model_settings()
                .strict_tools
                .get(id)
                .copied()
                .or(config.strict_tools);
            return model::Model::new(
                p.into(),
                m.into(),
                provider,
                config,
                if profile.primary_config.is_some() {
                    profile.primary_effort.as_deref()
                } else {
                    profile.effort.as_deref()
                },
            );
        }
        build_model(self.store.snapshot(), &self.model_settings())
    }
    fn model_settings(&self) -> SessionSettings {
        let mut settings = self.control.journal.lock().unwrap().settings.clone();
        if self.agent_id != "root" && settings.exclusive.is_none() {
            settings.model = self.model_override.clone();
            settings.effort = self.effort_override.clone();
        }
        settings
    }
    fn prepare(
        &self,
        model: &model::Model,
        definitions: &[tools::ToolDefinition],
        history: &[HistoryEvent],
    ) -> Result<model::PreparedRequest, EngineError> {
        let assets = self.control.journal.lock().unwrap().dir.join("assets");
        let cache_key = self.cache_key(model, definitions)?;
        crate::attachments::validate(&model.config.capabilities, history, Some(&assets))?;
        let mut request = if self.continuing {
            if let Some(profile) = &self.child_profile {
                let context = self.control.journal.lock().unwrap().contexts[&self.agent_id];
                if profile.provider_id == model.provider_id
                    && serde_json::from_slice::<serde_json::Value>(&profile.request.body)
                        .ok()
                        .is_some_and(|b| b["model"] == model.model_id)
                    && serde_json::to_value(definitions).ok()
                        == serde_json::to_value(&profile.request.offered_tools).ok()
                    && context == profile.context
                    && history.len() >= profile.history_len
                    && model::history_hash(&history[..profile.history_len]) == profile.history_hash
                {
                    let mut prior = profile.request.clone();
                    prior.assets_dir = Some(assets.clone());
                    prior.with_history_tail(&history[profile.history_len..])?
                } else {
                    model.prepare_with_assets(
                        &self.prompt(),
                        definitions,
                        history,
                        &cache_key,
                        Some(assets.clone()),
                    )?
                }
            } else {
                model.prepare_with_assets(
                    &self.prompt(),
                    definitions,
                    history,
                    &cache_key,
                    Some(assets.clone()),
                )?
            }
        } else {
            model.prepare_with_assets(
                &self.prompt(),
                definitions,
                history,
                &cache_key,
                Some(assets.clone()),
            )?
        };
        self.tools
            .lsp
            .set_model_override(&self.agent_id, model.config.lsp.clone());
        *self.tools.names.lock().unwrap() = self.tool_names();
        request.tool_aliases = self.tool_names().names;
        request.assets_dir = Some(assets);
        request.agent_id = self.agent_id.clone();
        request.session_id = self.id();
        // Reused legacy checkpoints keep the command identity they originally sent.
        if !request.conversation_id.is_empty() {
            request.conversation_id = format!("{}:{}", self.id(), self.agent_id);
        }
        request.capability_catalog = Some(
            self.context_state
                .frozen
                .as_ref()
                .and_then(|c| c.model_catalogs.get(&model.id()).cloned())
                .unwrap_or_else(|| self.available_definitions()),
        );
        if model.provider.kind == code_config::ProviderType::Command {
            let journal = self.control.journal.lock().unwrap();
            let record = &journal.agents[&self.agent_id];
            let mut depth = 0;
            let mut parent = record.parent_id.as_ref();
            while let Some(id) = parent {
                depth += 1;
                parent = journal.agents.get(id).and_then(|a| a.parent_id.as_ref());
            }
            request.command_context = model::command::RequestContext {
                cwd: self.cwd.clone(),
                state_dir: Some(self.store.directories.data.join("command-conversations")),
                parent_conversation_id: record
                    .parent_id
                    .as_ref()
                    .map(|id| format!("{}:{id}", journal.header.id))
                    .unwrap_or_default(),
                depth,
                agent: record.agent.clone(),
                permission_mode: serde_json::to_value(
                    journal
                        .settings
                        .mode
                        .unwrap_or(self.store.snapshot().config.permissions.mode),
                )
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "manual".into()),
                latest: model.command_latest(),
                output_file: None,
            };
        }
        request.enable_tools()?;
        Ok(request)
    }
    fn metadata_operation(
        &self,
        call: ToolCall,
        target: Option<PathBuf>,
    ) -> tools::PreparedOperation {
        let mut op = tools::PreparedOperation::metadata(call, self.cwd.clone(), target);
        op.agent_id = self.agent_id.clone();
        op.breadcrumb = self.control.breadcrumb(&self.agent_id);
        op.job_id = self.job_id.clone();
        op.parent_call_id = self.parent_call_id.clone();
        op
    }
    fn history(&self) -> Vec<HistoryEvent> {
        self.control
            .journal
            .lock()
            .unwrap()
            .histories
            .get(&self.agent_id)
            .cloned()
            .unwrap_or_default()
    }
    fn raw_definitions(&self) -> Vec<tools::ToolDefinition> {
        let mut definitions = ToolBox::definitions();
        definitions
            .retain(|d| d.name != "todo" && (d.name != "lsp" || self.control.lsp.available()));
        definitions.retain(|d| d.name != "terminal" || self.control.user_terminal_allowed());
        definitions.push(script::definition());
        definitions.extend(session_tools::definitions());
        definitions.extend(tools::web::definitions());
        if tools::generate::offered(&self.config.lock().unwrap()) {
            definitions.push(tools::generate::definition());
        }
        definitions.extend(tree::definitions(
            self.current_remaining_depth(),
            self.agent_id != "root",
        ));
        tool_profiles::add_hints(&mut definitions);
        definitions
    }
    fn live_definitions(&self) -> Vec<tools::ToolDefinition> {
        let mut definitions = self.raw_definitions();
        if let Ok(model) = self.model() {
            tools::media::filter(&mut definitions, &model.config.capabilities);
        }
        let overrides = self.tool_overrides();
        let mut definitions = tool_names::apply(definitions, &overrides).unwrap_or_default();
        if self.control.flat_mode() {
            crate::flat::filter_tools(
                &mut definitions,
                self.agent_id != "root",
                &tool_names::names(&overrides),
            );
        }
        definitions
    }
    /// Frozen allowed capabilities, including extensions omitted from native tools.
    fn available_definitions(&self) -> Vec<tools::ToolDefinition> {
        if let Some(context) = &self.context_state.frozen {
            let id = self.model().map(|m| m.id()).unwrap_or_default();
            if let Some(catalog) = context.model_catalogs.get(&id) {
                return catalog.clone();
            }
            // Legacy sessions freeze their historical native surface as the catalog.
            if context.model_catalogs.is_empty()
                && let Some(native) = context.model_tools.get(&id)
            {
                return native.clone();
            }
        }
        if let Some(profile) = &self.child_profile {
            return profile.request.catalog().to_vec();
        }
        self.live_definitions()
    }
    fn native_definitions(&self, catalog: &[tools::ToolDefinition]) -> Vec<tools::ToolDefinition> {
        let id = self.model().map(|m| m.id()).unwrap_or_default();
        let profiles = self
            .context_state
            .frozen
            .as_ref()
            .map(|c| c.tool_profiles.clone())
            .unwrap_or_else(|| tool_profiles::snapshots(self.store.snapshot()));
        tool_profiles::native(
            catalog,
            tool_profiles::selected(&profiles, &id),
            &self.tool_names(),
        )
    }
    fn definitions(&self) -> Vec<tools::ToolDefinition> {
        if let Some(defs) = self.context_state.frozen.as_ref().and_then(|c| {
            self.model()
                .ok()
                .and_then(|m| c.model_tools.get(&m.id()).cloned())
        }) {
            return defs;
        }
        self.context_state
            .frozen
            .as_ref()
            .and_then(|c| c.offered_tools.clone())
            .or_else(|| {
                self.child_profile
                    .as_ref()
                    .map(|p| p.request.offered_tools.clone())
            })
            .unwrap_or_else(|| self.native_definitions(&self.available_definitions()))
    }
    fn current_remaining_depth(&self) -> Option<usize> {
        if self.control.flat_mode() {
            return Some(usize::from(self.agent_id == "root"));
        }
        let max = self.config.lock().unwrap().config.subagents.max_depth;
        let journal = self.control.journal.lock().unwrap();
        let mut depth = 0usize;
        let mut id = self.agent_id.as_str();
        while let Some(parent) = journal.agents.get(id).and_then(|a| a.parent_id.as_deref()) {
            depth += 1;
            id = parent;
        }
        max.map(|n| n.saturating_sub(depth))
    }
    fn prompt(&self) -> String {
        let model = self.model().ok();
        let prompt = self
            .context_state
            .frozen
            .as_ref()
            .map(|c| c.prompt(&model.as_ref().map(|m| m.id()).unwrap_or_default()))
            .or_else(|| self.child_profile.as_ref().map(|p| p.system.clone()))
            .unwrap_or_else(|| {
                self.control.journal.lock().unwrap().agents[&self.agent_id]
                    .stable_prompt
                    .clone()
            });
        // D114: the agent knows its own model. Last in the prompt so a model switch
        // (which starts a new cache anyway) only changes the tail; effort is left out
        // so effort changes keep the cache.
        match model {
            Some(m) => {
                let label = m
                    .config
                    .label
                    .as_deref()
                    .map(|l| format!(" ({l})"))
                    .unwrap_or_default();
                format!("{prompt}\nYou are running as model `{}`{label}.\n", m.id())
            }
            None => prompt,
        }
    }
    fn freeze_context(&mut self) -> Result<(), EngineError> {
        if self.context_state.frozen.is_some() {
            return self.freeze_tool_model(&self.model()?);
        }
        let (existing, shared, global, role) = {
            let journal = self.control.journal.lock().unwrap();
            let role = journal.agents[&self.agent_id].stable_prompt.clone();
            let existing = journal.histories.get(&self.agent_id).and_then(|h| {
                h.iter().rev().find_map(|e| match e {
                    HistoryEvent::PromptContext { context } if context.cwd == self.cwd => {
                        Some(*context.clone())
                    }
                    _ => None,
                })
            });
            let shared = journal
                .histories
                .values()
                .flat_map(|h| h.iter())
                .find_map(|e| match e {
                    HistoryEvent::PromptContext { context } if context.cwd == self.cwd => {
                        Some(*context.clone())
                    }
                    _ => None,
                });
            let global = journal.histories.get("root").and_then(|h| {
                h.iter().find_map(|e| match e {
                    HistoryEvent::PromptContext { context } => Some(*context.clone()),
                    _ => None,
                })
            });
            (existing, shared, global, role)
        };
        let mut context = if let Some(existing) = existing {
            existing
        } else {
            let mut context = if let Some(mut shared) = shared {
                shared.role_prompt = role;
                shared.offered_tools = None;
                shared.model_tools.clear();
                shared.offered_catalog = None;
                shared.model_catalogs.clear();
                shared
            } else {
                context::FrozenContext::capture(&self.store, &self.cwd, role, global.as_ref())?
            };
            context.tool_model = self.model().ok().map(|m| m.id());
            let overrides = tool_names::frozen(
                &context.tool_overrides,
                context.tool_model.as_deref().unwrap_or(""),
            );
            tool_names::apply(tool_defaults::definitions(), &overrides)?;
            self.context_state.frozen = Some(context.clone());
            let catalog = self.live_definitions();
            context.offered_tools = Some(self.native_definitions(&catalog));
            context.offered_catalog = Some(catalog);
            self.control.commit(
                &self.agent_id,
                HistoryEvent::PromptContext {
                    context: Box::new(context.clone()),
                },
            )?;
            context
        };
        if context.model_tools.is_empty() {
            let raw = self.raw_definitions();
            for (provider, p) in &self.store.snapshot().providers {
                for (model, config) in &p.models {
                    let mut raw = raw.clone();
                    tools::media::filter(&mut raw, &config.capabilities);
                    let id = format!("{provider}/{model}");
                    let overrides = tool_names::frozen(&context.tool_overrides, &id);
                    let mut definitions = tool_names::apply(raw.clone(), &overrides)?;
                    if self.control.flat_mode() {
                        crate::flat::filter_tools(
                            &mut definitions,
                            self.agent_id != "root",
                            &tool_names::names(&overrides),
                        );
                    }
                    let native = tool_profiles::native(
                        &definitions,
                        tool_profiles::selected(&context.tool_profiles, &id),
                        &tool_names::names(&overrides),
                    );
                    context.model_catalogs.insert(id.clone(), definitions);
                    context.model_tools.insert(id, native);
                }
            }
            if let (Some(id), Some(defs)) = (&context.tool_model, &context.offered_tools) {
                context.model_tools.insert(id.clone(), defs.clone());
                context.model_catalogs.insert(
                    id.clone(),
                    context
                        .offered_catalog
                        .clone()
                        .unwrap_or_else(|| defs.clone()),
                );
            }
            self.control.commit(
                &self.agent_id,
                HistoryEvent::PromptContext {
                    context: Box::new(context.clone()),
                },
            )?;
        }
        if context.model_catalogs.is_empty() {
            // D61 migration: never reinterpret an already-sent profile on resume.
            context.model_catalogs = context.model_tools.clone();
            context.offered_catalog = context.offered_tools.clone();
        }
        for path in context.source_hashes.keys().filter(|p| p.is_file()) {
            if let Ok(bytes) = std::fs::read(path)
                && context.source_hashes.get(path).and_then(|h| h.as_ref())
                    == Some(&context::digest(&bytes))
            {
                self.context_state
                    .source_contents
                    .lock()
                    .unwrap()
                    .remember(path, &bytes);
                self.tools.watch_source(path, &bytes);
            }
        }
        self.context_state.observed_hashes = Some(context.source_hashes.clone());
        self.context_state.frozen = Some(context);
        self.freeze_tool_model(&self.model()?)
    }
    /// A model discovered after session creation gets its role surface exactly
    /// once, before its first request. Existing model surfaces are never rebuilt.
    fn freeze_tool_model(&mut self, model: &model::Model) -> Result<(), EngineError> {
        let Some(context) = self.context_state.frozen.as_ref() else {
            return Ok(());
        };
        let id = model.id();
        if context.model_tools.contains_key(&id) {
            return Ok(());
        }
        let mut context = context.clone();
        let overrides = tool_names::frozen(&context.tool_overrides, &id);
        let names = tool_names::names(&overrides);
        let mut raw = self.raw_definitions();
        tools::media::filter(&mut raw, &model.config.capabilities);
        let mut catalog = tool_names::apply(raw, &overrides)?;
        if self.control.flat_mode() {
            flat::filter_tools(&mut catalog, self.agent_id != "root", &names);
        }
        let native = tool_profiles::native(
            &catalog,
            tool_profiles::selected(&context.tool_profiles, &id),
            &names,
        );
        context.model_catalogs.insert(id.clone(), catalog);
        context.model_tools.insert(id.clone(), native);
        context.tool_overrides.insert(id, overrides);
        self.control.commit(
            &self.agent_id,
            HistoryEvent::PromptContext {
                context: Box::new(context.clone()),
            },
        )?;
        self.context_state.frozen = Some(context);
        Ok(())
    }
    fn boundary_notices(&mut self) -> Result<(), EngineError> {
        self.control.deliver_subscriptions(&self.agent_id)?;
        self.control.deliver_growth(&self.agent_id)?;
        self.freeze_context()?;
        if self
            .available_definitions()
            .iter()
            .any(|d| d.schema["x-cockpit-user-terminal"] == true)
            && !self.control.user_terminal_allowed()
        {
            return Err(EngineError::bad(
                "this frozen session requires an interactive terminal frontend; headless, ACP and unattended use require a new session",
            ));
        }
        let context = self.context_state.frozen.as_ref().unwrap();
        let observed = context.changes(&self.store)?;
        let previous = self.context_state.observed_hashes.as_ref().unwrap();
        let mut changed = observed
            .keys()
            .chain(previous.keys())
            .filter(|p| observed.get(*p) != previous.get(*p))
            .cloned()
            .collect::<Vec<_>>();
        changed.sort();
        changed.dedup();
        changed.retain(|path| !self.tools.own_revision(path));
        if !changed.is_empty() {
            let diffs = changed
                .iter()
                .map(|p| {
                    self.tools.read_notice(p).unwrap_or_else(|| {
                        self.context_state.source_contents.lock().unwrap().notice(p)
                    })
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text:format!("{diffs}\nPrompt source files changed: {}. The session system prompt and skill catalog remain frozen. Read changed files or use skills.read for current contents; start a new session to rebuild context.", serde_json::json!(changed)) })?;
        }
        self.context_state.observed_hashes = Some(observed);
        const SCRIPT_STATE_NOTICE: &str = "Monty state helpers are available through script: store(key,value) and load(key). Python locals/functions still start fresh each call. Values are per-agent JSON, capped at 1 MiB total/1024 keys (keys 1–256 bytes), cleared on agent end or process restart; missing keys return None. Existing tools[] and system prompt remain fixed.";
        if self.available_definitions().iter().any(|tool| self.tool_names().canonical(&tool.name) == "script")
            && !self.history().iter().any(|event| matches!(event, HistoryEvent::SystemReminder { text } if text.contains("Monty state helpers are available")))
        {
            self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text: self.tool_names().rewrite(SCRIPT_STATE_NOTICE) })?;
        }
        const CRITERIA_GUIDANCE: &str = "Use criteria only for multi-step work or work needing proof; trivial tasks just do the work. A child may create criteria only when its parent passed criteria through task parameters, and may update and meet passed ones. Only long-running children should send partial reports. Review panels run only when acceptance or findings criteria exist.";
        if !context.shared_prompt.contains("Use criteria only for multi-step work or work needing proof")
            && !context.role_prompt.contains("Use criteria only for multi-step work or work needing proof")
            && !self.history().iter().any(|event| matches!(event, HistoryEvent::SystemReminder { text } if injection::body(text).unwrap_or(text) == CRITERIA_GUIDANCE))
        {
            self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text: CRITERIA_GUIDANCE.into() })?;
        }
        if !self.context_state.announced_capabilities {
            if self.control.flat_mode() {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder {
                    text: if self.agent_id == "root" {
                        "This session uses flat subagents, fixed for the session. task defaults to foreground; multiple native task calls in one response run in parallel and the turn waits for all. background:true plus jobs remains available. Children cannot spawn, and their questions go directly to the user. Root agents list/show/ask/stop, scopes, model policy and decisions remain available. No maxDescendants, finalize, growth notices or partial reports."
                    } else {
                        "You are a flat-mode child. You cannot spawn or use tree controls; task, agents, message, answer, escalate, models and partial report are unavailable, including through script. Use question to ask the user directly; your parent receives questions/answers in your final report. Scope, jobs, decide, criteria and your own compact remain available. Finish with one final report to your parent."
                    }.into(),
                })?;
            }
            if !self.definitions().iter().any(|tool| {
                self.tool_names().canonical(&tool.name) == "criteria"
                    && tool.schema["properties"].get("unverified").is_some()
            }) {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder {
                    text: "Your native criteria schema is an older frozen definition. To use current findings fields, call the script tool with code: print(tools.describe(name='criteria')). Then use script code: print(criteria.finding(id='<id>', text='<problem>', evidence='<evidence>', severity='low', locations=['file:1'], fix='<fix>', verification='<how checked>', confidence=0.8)). Do not send these new fields to the old native criteria tool. Findings met also requires unverified=[] and notCovered=[], including checkedClean areas.".into(),
                })?;
            }
            if self.agent_id != "root"
                && !self.control.flat_mode()
                && !self
                    .definitions()
                    .iter()
                    .any(|tool| self.tool_names().canonical(&tool.name) == "report")
            {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder {
                    text: "Your native tools are an older frozen definition without report. Only long-running children should send partial progress. Use the script tool with code: print(report(partial=True, text='Progress and evidence so far')). report is a standalone child tool, not an action of task or agents. Continue work after reporting; write the final report as your final answer. The parent receives the latest partial and an idle root wakes.".into(),
                })?;
            }
            if !context.shared_prompt.contains(context::HEADROOM_GUIDANCE)
                && !self.history().iter().any(|e| matches!(e, HistoryEvent::SystemReminder { text } if text.contains(context::HEADROOM_GUIDANCE))) {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text: context::HEADROOM_GUIDANCE.into() })?;
            }
            let offered = self.definitions();
            if offered.iter().any(|tool| {
                self.tool_names().canonical(&tool.name) == "task"
                    && tool.schema["properties"].get("title").is_none()
            }) {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder {
                    text: "Your native task schema is an older frozen definition without title. For current task fields, use the script tool with code: print(tools.describe(name='task')); then print(task(title='Short title', prompt='Bounded brief', background=True)). Do not send title or other new fields to the old native task tool. Long-running tree-mode children have a standalone report(partial=True, text=...) tool for useful mid-run progress to their parent; final reports are final answers.".into(),
                })?;
            }
            if !offered.iter().any(|tool| tool.name == "criteria")
                || offered.iter().any(|tool| {
                    tool.name == "task" && tool.schema["properties"].get("acceptance").is_none()
                })
            {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder {
                    text: "Criteria contracts and task criteria/review are available through Monty for multi-step work or work needing proof; trivial tasks just do the work. Children may create criteria only if their parent passed them. Use tools.describe(name=\"criteria\") and tools.describe(name=\"task\"), then criteria.show(), criteria.add(...), criteria.met(...), criteria.propose(...) and task(..., acceptance=[...], review=True). The former goals functions are retired; steps belong to criteria. Your existing tools[] and system prompt remain fixed.".into(),
                })?;
            }
            if !offered.iter().any(|tool| {
                self.tool_names().canonical(&tool.name) == "scope"
                    && tool.schema["properties"]["action"]["enum"]
                        .as_array()
                        .is_some_and(|actions| actions.iter().any(|a| a == "subscribe"))
            }) || offered.iter().any(|tool| {
                self.tool_names().canonical(&tool.name) == "task"
                    && tool.schema["properties"].get("subscribe").is_none()
            }) {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text: "Write scopes and subscriptions are available through Monty: scope.show(), scope.requests(), scope.resolve(requestId=..., decision=apply/reject/grant, reason=..., scope=[...]), scope.wait(requestId=...), scope.subscribe(subscribe=[dirs]). task accepts scope=[dirs] and subscribe=[dirs]; write/edit accept wait=True to wait explicitly. Bash is unrestricted by scopes. Existing tools[] stay fixed.".into() })?;
            }
            if !self.control.flat_mode()
                && !offered.iter().any(|tool| {
                    self.tool_names().canonical(&tool.name) == "agents"
                        && tool.schema["properties"]["action"]["enum"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|a| a == "finalize"))
                })
            {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text: "Agent tree navigation is available through Monty: agents.list(state=...,context='earlier',subtree='mine',model=...,query=...,limit=...), agents.show(id=...), agents.ask(id=...,question=...), agents.stop(id=...,reason=...), agents.finalize(id=...). Finalize wraps up bottom-up after current calls finish; each node gets one final report turn with tools disabled. Read the whole tree; ask/stop/finalize target descendants only. message also accepts your direct parent (to=parent or its ID); more distant ancestors and other branches are refused. task now accepts a durable title (1–80 characters), maxDescendants and inherited models={allow:[patterns],deny:[patterns]}. Use tools.describe(name='agents') and tools.describe(name='task'). Existing tools[] remain fixed.".into() })?;
            }
            if !offered
                .iter()
                .any(|tool| self.tool_names().canonical(&tool.name) == "decide")
            {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text: "Decision recording is available through Monty: decide(question=...,choice=...,options=[...],why=...,files=[...],criteria=[...]). Root may inspect decisions.list() and decisions.search(query=...). Scope, criteria resolutions, safety verdicts, fallbacks and subtree stops are recorded automatically. Existing tools[] remain fixed.".into() })?;
            }
            let missing = ["lsp", "code", "graph", "context_pack", "change_impact"]
                .into_iter()
                .filter(|name| {
                    self.available_definitions()
                        .iter()
                        .any(|tool| self.tool_names().canonical(&tool.name) == *name)
                })
                .filter(|name| {
                    !offered
                        .iter()
                        .any(|tool| self.tool_names().canonical(&tool.name) == *name)
                })
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text:format!("Code intelligence capabilities are now available through Monty: {}. Use tools.describe(name=...) to inspect schemas, then call them inside script. Your existing tools[] remains fixed. Relationships are heuristic; confirm behavior in source.", missing.join(", ")) })?;
            }
            let outdated = self.definitions().iter().any(|tool| {
                (tool.name == "question" && tool.schema["properties"].get("questions").is_none())
                    || (tool.name == "answer" && tool.schema["properties"].get("answers").is_none())
            });
            if outdated {
                self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text:"Structured question sets and answers are now available through Monty. Use tools.describe(name=\"question\") and tools.describe(name=\"answer\") for their current schemas, then call question(questions=[...], wait=...) or answer(questionId=..., answers=[...]) in script.".into() })?;
            }
            self.context_state.announced_capabilities = true;
        }
        let date = chrono::Local::now().format("%Y-%m-%d %Z").to_string();
        if self.context_state.date != date {
            let mode = self
                .control
                .journal
                .lock()
                .unwrap()
                .settings
                .mode
                .unwrap_or(self.store.snapshot().config.permissions.mode);
            self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text:format!("Current date: {date}. cwd: {}. Permission mode: {:?}. Enabled MCP servers: {}. MCP tools are reachable through script; discover with tools.search(query=...).{}", self.cwd.display(), mode, serde_json::json!(self.control.mcp.statuses().into_iter().filter(|s|s.enabled).map(|s|s.name).collect::<Vec<_>>()), if self.agent_id != "root" { " You are a noninteractive subagent. Finish with a concise report for your parent; use question with wait=blocking when you cannot make progress without an answer. Your cwd is your inside-cwd permission boundary." } else { "" }) })?;
            self.context_state.date = date;
        }
        let (todos, changed_files) = self.tools.context_observations();
        let todos = todos.to_string();
        if todos != self.context_state.todos
            && (todos != "[]" || !self.context_state.todos.is_empty())
        {
            self.control.commit(
                &self.agent_id,
                HistoryEvent::SystemReminder {
                    text: format!("Current todo state: {todos}"),
                },
            )?;
        }
        self.context_state.todos = todos;
        let signature = serde_json::to_string(&changed_files).unwrap();
        if signature != self.context_state.external_files && !changed_files.is_empty() {
            self.control.commit(
                &self.agent_id,
                HistoryEvent::SystemReminder {
                    text: format!(
                        "{}\nRe-read before editing; the stale-write check remains in force.",
                        changed_files
                            .iter()
                            .map(|(_, _, diff)| diff.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                },
            )?;
        }
        self.context_state.external_files = signature;
        let remaining = self.current_remaining_depth();
        if self.remaining_depth != remaining {
            let was_available = self.remaining_depth != Some(0);
            let available = remaining != Some(0);
            self.remaining_depth = remaining;
            self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text:format!("Subagent depth changed: remaining depth {}. task is {}{}.", serde_json::json!(remaining), if available { "available through script (the original model-facing schema remains fixed)" } else { "unavailable; direct and script calls are refused" }, if was_available != available { " (capability changed)" } else { "" }) })?;
        }
        for usage in self.control.usage.snapshot() {
            for warning in usage.warnings {
                let marker = format!(
                    "{} {}",
                    warning,
                    usage
                        .budget
                        .as_ref()
                        .and_then(|b| b.resets_at.as_deref())
                        .unwrap_or("")
                );
                if self.context_state.budget_warnings.insert(marker) {
                    self.control.commit(
                        &self.agent_id,
                        HistoryEvent::SystemReminder { text: warning },
                    )?;
                }
            }
        }
        self.control.mcp.poll_health();
        for text in self.control.mcp.take_notices() {
            let recipients = self
                .control
                .journal
                .lock()
                .unwrap()
                .agents
                .values()
                .filter(|a| !a.hidden())
                .map(|a| a.agent_id.clone())
                .collect::<Vec<_>>();
            for recipient in recipients {
                self.control.commit(
                    &recipient,
                    HistoryEvent::Message {
                        from: "mcp".into(),
                        to: recipient.clone(),
                        text: text.clone(),
                    },
                )?;
            }
        }
        Ok(())
    }
    fn budget_notice(
        &mut self,
        request: &model::PreparedRequest,
        model: &model::Model,
    ) -> Result<bool, EngineError> {
        if model.command_latest() {
            return Ok(false);
        }
        let used = self.context_used(request.input_estimate).0;
        let Some(window) = model.config.context_window else {
            return Ok(false);
        };
        let bucket = if used >= window * 9 / 10 {
            90
        } else if used >= window * 3 / 4 {
            75
        } else if used >= window / 2 {
            50
        } else {
            0
        };
        if bucket <= self.context_state.budget_bucket {
            return Ok(false);
        }
        self.control.commit(&self.agent_id, HistoryEvent::SystemReminder { text:format!("Context token budget: approximately {used}/{window} input tokens used; reserve {} for output. Preserve the contract and open criteria and consider compact with a focus.", model.config.max_output_tokens.unwrap_or(8192)) })?;
        self.context_state.budget_bucket = bucket;
        Ok(true)
    }
    fn id(&self) -> String {
        self.control.journal.lock().unwrap().header.id.clone()
    }
    fn cache_key(
        &self,
        model: &model::Model,
        definitions: &[tools::ToolDefinition],
    ) -> Result<String, EngineError> {
        use sha2::{Digest, Sha256};
        let role = if self.agent_id == "root" {
            "root"
        } else if self.control.flat_mode() {
            "flat_child"
        } else {
            "tree_child"
        };
        let profiles = self
            .context_state
            .frozen
            .as_ref()
            .map(|c| c.tool_profiles.clone())
            .unwrap_or_else(|| tool_profiles::snapshots(self.store.snapshot()));
        let profile = match tool_profiles::selected(&profiles, &model.id()) {
            code_config::tools::Profile::Full => "full",
            code_config::tools::Profile::Core => "core",
        };
        let prefix = serde_json::to_vec(&(self.prompt(), definitions))
            .map_err(|e| EngineError::bad(e.to_string()))?;
        let hash = format!("{:x}", Sha256::digest(&prefix));
        Ok(format!(
            "{}:{role}:{}:{profile}:{}",
            self.id(),
            model.id(),
            &hash[..16]
        ))
    }
    fn pair_interrupted_calls(&self) -> Result<(), EngineError> {
        // Only the latest assistant can be pending; earlier turns were paired before advancing.
        let history = self.history();
        let Some(index) = history
            .iter()
            .rposition(|e| matches!(e, HistoryEvent::Assistant { .. }))
        else {
            return Ok(());
        };
        let HistoryEvent::Assistant { tool_calls, .. } = &history[index] else {
            unreachable!()
        };
        for call in tool_calls {
            let paired = history[index + 1..]
                .iter()
                .any(|e| matches!(e, HistoryEvent::ToolResult { id, .. } if id == &call.id));
            if !paired {
                self.control.commit(&self.agent_id, HistoryEvent::ToolResult { details: Default::default(),
                    id: call.id.clone(), name: call.name.clone(),
                    output: "Error: previous process stopped before recording this tool's outcome; inspect state before retrying".into(),
                    is_error: true,
                })?;
            }
        }
        Ok(())
    }
    fn drain_mailbox(&self) -> Result<(), EngineError> {
        self.drain_local_commands()?;
        self.deliver_queue(false)?;
        let mut mail_since = self.control.mail_since.lock().unwrap();
        let messages = self
            .control
            .journal
            .lock()
            .unwrap()
            .mailbox
            .get(&self.agent_id)
            .cloned()
            .unwrap_or_default();
        mail_since.remove(&self.agent_id);
        drop(mail_since);
        for text in messages {
            self.control
                .commit(&self.agent_id, HistoryEvent::SystemReminder { text })?;
        }
        Ok(())
    }
    async fn stream_model(
        &self,
        model: &model::Model,
        request: &model::PreparedRequest,
        tx: &EventSender,
        cancel: &CancellationToken,
    ) -> Result<model::Completion, EngineError> {
        let (_short, _stream) = if self.control.scheduler.contains(&self.agent_id) {
            self.control
                .scheduler
                .retarget(&self.agent_id, scheduler::Target::model(model));
            (
                None,
                Some(self.control.scheduler.enter(&self.agent_id, cancel).await?),
            )
        } else {
            (
                Some(
                    self.control
                        .scheduler
                        .short(&self.agent_id, model, cancel)
                        .await?,
                ),
                None,
            )
        };
        let (wire_tx, mut wire_rx) = mpsc::unbounded_channel();
        let stream_cancel = cancel.child_token();
        let mut interrupted = false;
        let mut text = String::new();
        let mut capture = |event: EngineEvent| -> Result<(), EngineError> {
            match &event {
                EngineEvent::Text { delta } => text.push_str(delta),
                EngineEvent::StreamReset { .. } => text.clear(),
                _ => {}
            }
            self.model_event(event, model, tx)
        };
        let result = {
            let future = model.stream(request, &wire_tx, &stream_cancel);
            tokio::pin!(future);
            loop {
                let changed = self.control.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !interrupted && self.control.has_interrupt(&self.agent_id) {
                    interrupted = true;
                    stream_cancel.cancel();
                }
                tokio::select! {
                    result = &mut future => break result,
                    event = wire_rx.recv() => { if let Some(event) = event { capture(event)?; } },
                    _ = &mut changed => {},
                }
            }
        };
        drop(wire_tx);
        while let Some(event) = wire_rx.recv().await {
            capture(event)?;
        }
        if interrupted && !cancel.is_cancelled() {
            return Ok(model::Completion {
                text,
                status: CompletionStatus::Cancelled,
                stop_reason: "interrupted".into(),
                ..Default::default()
            });
        }
        result
    }
    fn model_event(
        &self,
        event: EngineEvent,
        model: &model::Model,
        tx: &EventSender,
    ) -> Result<(), EngineError> {
        if let EngineEvent::Context { meter } = &event
            && self.agent_id == "root"
        {
            *self.control.meter.lock().unwrap() = meter.clone();
        }
        let mut reported = None;
        if let EngineEvent::Usage { usage } = &event {
            self.control.commit(
                &usage.agent_id,
                HistoryEvent::Usage {
                    usage: usage.clone(),
                },
            )?;
            // Author usage is authoritative immediately, including on the final
            // response. Auxiliary calls and failed attempts do not replace it.
            if usage.kind == "agent"
                && usage.agent_id == self.agent_id
                && usage.model == model.id()
                && !usage.partial
            {
                let context = if let Some(used) = usage.context_used {
                    if model.command_latest() && usage.context_window.is_none() {
                        None
                    } else {
                        Some((used, usage.context_window.or(model.config.context_window)))
                    }
                } else if !model.command_latest() {
                    usage.tokens.input_tokens.map(|input| {
                        (
                            input.saturating_add(usage.tokens.output_tokens.unwrap_or(0)),
                            usage.context_window.or(model.config.context_window),
                        )
                    })
                } else {
                    None
                };
                if let Some((used, window)) = context {
                    let output = model.config.max_output_tokens.unwrap_or(8192);
                    let meter = ContextMeter {
                        used,
                        window,
                        output_reservation: output,
                        source: "provider".into(),
                        warning: window.is_some_and(|w| {
                            used.saturating_add(output) >= w.saturating_mul(9) / 10
                        }),
                    };
                    if self.agent_id == "root" {
                        *self.control.meter.lock().unwrap() = meter.clone();
                    }
                    reported = Some(meter);
                }
            }
        }
        let _ = tx.send(event);
        if let Some(meter) = reported {
            let _ = tx.send(EngineEvent::Context { meter });
        }
        Ok(())
    }
    fn model_reminder(&self, model: &model::Model) -> Result<(), EngineError> {
        let Some(steering) = self
            .context_state
            .frozen
            .as_ref()
            .and_then(|c| c.model_steering.get(&model.id()))
        else {
            return Ok(());
        };
        let Some(text) = &steering.reminder else {
            return Ok(());
        };
        let turn = {
            let j = self.control.journal.lock().unwrap();
            let count = j.model_turns.get(&self.agent_id).copied().unwrap_or(0);
            let previous = j
                .model_reminders
                .get(&(self.agent_id.clone(), model.id()))
                .copied()
                .unwrap_or(0);
            (count >= previous.saturating_add(steering.reminder_turns.max(1))).then_some(count)
        };
        if let Some(turn) = turn {
            self.control.commit(
                &self.agent_id,
                HistoryEvent::SystemReminder {
                    text: format!("Model reminder: {text}"),
                },
            )?;
            self.control.commit(
                &self.agent_id,
                HistoryEvent::ModelReminder {
                    model: model.id(),
                    turn,
                },
            )?;
        }
        Ok(())
    }
    fn context_used(&self, estimate: u64) -> (u64, &'static str) {
        if let Some((input, previous)) = self.anchor {
            (
                (input as i128 + estimate as i128 - previous as i128).max(0) as u64,
                "provider+tokenizer",
            )
        } else {
            (estimate, "tokenizer")
        }
    }
    fn meter(
        &self,
        request: &model::PreparedRequest,
        model: &model::Model,
    ) -> Result<(), EngineError> {
        if model.command_latest() {
            let history = self.history();
            let usage = history.iter().rev().find_map(|h| match h {
                HistoryEvent::Usage { usage }
                    if usage.model == model.id() && usage.kind == "agent" =>
                {
                    Some(usage)
                }
                _ => None,
            });
            let reported = usage.and_then(|u| u.context_used.zip(u.context_window));
            let meter = ContextMeter {
                used: reported.map_or(0, |(u, _)| u),
                window: reported.map(|(_, w)| w),
                output_reservation: model.config.max_output_tokens.unwrap_or(8192),
                source: if reported.is_some() {
                    "provider"
                } else {
                    "unknown"
                }
                .into(),
                warning: reported.is_some_and(|(u, w)| u >= w.saturating_mul(9) / 10),
            };
            if self.agent_id == "root" {
                *self.control.meter.lock().unwrap() = meter.clone();
            }
            self.control
                .emit_for(&self.agent_id, EngineEvent::Context { meter });
            return Ok(());
        }
        let (used, source) = self.context_used(request.input_estimate);
        let output = model.config.max_output_tokens.unwrap_or(8192);
        let meter = ContextMeter {
            used,
            window: model.config.context_window,
            output_reservation: output,
            source: source.into(),
            warning: model
                .config
                .context_window
                .is_some_and(|w| used.saturating_add(output) >= w * 9 / 10),
        };
        if self.agent_id == "root" {
            *self.control.meter.lock().unwrap() = meter.clone();
        }
        self.control
            .emit_for(&self.agent_id, EngineEvent::Context { meter });
        if model
            .config
            .context_window
            .is_some_and(|window| used.saturating_add(output) >= window)
        {
            return Err(EngineError {
                kind: ErrorKind::ContextOverflow,
                message:
                    "context limit reached; use /compact, fork, a larger model, or a new session"
                        .into(),
                retryable: false,
                retry_after_secs: None,
            });
        }
        Ok(())
    }
}
async fn run(
    state: &mut State,
    prompt: Option<(String, Vec<Attachment>)>,
    tx: &EventSender,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
) -> Result<String, EngineError> {
    state.control.check_owner()?;
    *state.guidance_turn.lock().unwrap() = guidance_selector::Turn::default();
    state.control.agent_status(&state.agent_id, "running")?;
    state.pair_interrupted_calls()?;
    state.deliver_queue(state.agent_id != "root")?;
    // D122: a local command the user ran reads before their next prompt.
    state.drain_local_commands()?;
    if let Some((prompt, attachments)) = prompt {
        state.control.commit(
            &state.agent_id,
            HistoryEvent::User {
                text: prompt,
                attachments,
            },
        )?;
    } else {
        state.drain_mailbox()?;
    }
    state.reload()?;
    state.await_model(cancel).await?;
    if state.agent_id == "root"
        && !state
            .control
            .journal
            .lock()
            .unwrap()
            .subscriptions
            .contains_key("root")
    {
        let paths = state.config.lock().unwrap().config.subscribe.clone();
        if !paths.is_empty() {
            state.control.set_subscriptions("root", &paths)?;
        }
    }
    state
        .control
        .project_reviewer_guidance(&state.cwd, cancel)
        .await?;
    let _run_slot = if state.agent_id == "root" {
        Some(state.control.scheduler.register(
            state.agent_id.clone(),
            state.agent_id.clone(),
            scheduler::Target::model(&state.model()?),
            None,
        ))
    } else {
        None
    };
    state.boundary_notices()?;
    state.control.commit(
        &state.agent_id,
        HistoryEvent::SystemReminder {
            text: state.control.scope_context_snapshot(&state.agent_id),
        },
    )?;
    if state.agent_id == "root" {
        for request in state
            .control
            .journal
            .lock()
            .unwrap()
            .scope_requests
            .values()
            .filter(|r| matches!(r.status.as_str(), "pending" | "applying"))
            .cloned()
            .collect::<Vec<_>>()
        {
            state
                .control
                .emit(EngineEvent::ScopeRequestUpdate { request });
        }

        for question in state
            .control
            .journal
            .lock()
            .unwrap()
            .questions
            .values()
            .filter(|q| q.answer.is_none() && q.recipient.is_none())
            .cloned()
            .collect::<Vec<_>>()
        {
            state.control.emit(EngineEvent::Question { question });
        }
    }
    let mut definitions = state.definitions();
    let mut guard = loop_guard::LoopGuard::new(&definitions);
    // Native thinking configuration must stay fixed across one assistant tool-use turn.
    state.active_model = None;
    let mut turn_model = state.model()?;
    let primary_model = turn_model.clone();
    let fallback_refs = turn_model.config.fallback.clone();
    let mut billing_providers = std::collections::BTreeSet::new();
    let mut fallback_tried = std::collections::BTreeSet::from([turn_model.id()]);
    let mut finish_warned = false;
    let mut criteria_warned = false;
    let mut empty_report_warned = false;
    let mut final_turn = false;
    loop {
        if !final_turn && state.control.is_finalizing(&state.agent_id) {
            state
                .control
                .finalize_boundary(&state.agent_id, cancel)
                .await?;
            final_turn = true;
        }
        if !final_turn
            && state
                .control
                .contract(&state.agent_id)
                .roadblock()
                .is_some()
        {
            return Ok("roadblocked".into());
        }
        if state.agent_id == "root" && state.control.goal_limits()? {
            return Ok("goal-paused".into());
        }
        if cancel.is_cancelled() {
            return Err(EngineError::cancelled());
        }
        if !final_turn && state.control.discussing(&state.agent_id) {
            state.control.scheduler.park(&state.agent_id);
            if state.agent_id == "root" {
                return Ok("discuss".into());
            }
            state.control.agent_status(&state.agent_id, "waiting")?;
            loop {
                let changed = state.control.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !state.control.discussing(&state.agent_id) {
                    break;
                }
                tokio::select! { _ = &mut changed => {}, _ = cancel.cancelled() => return Err(EngineError::cancelled()) }
            }
            state.control.scheduler.wake(&state.agent_id);
        }
        state.reload()?;
        exclusive::check_mode(
            &state.model_settings(),
            state
                .model_settings()
                .mode
                .unwrap_or(state.store.snapshot().config.permissions.mode),
        )?;
        if exclusive::check_model(&state.model_settings(), &turn_model).is_err() {
            state.active_model = None;
            turn_model = state.model()?;
            state
                .control
                .scheduler
                .retarget(&state.agent_id, scheduler::Target::model(&turn_model));
        }
        state
            .control
            .scheduler
            .acquire(&state.agent_id, cancel)
            .await?;
        if !final_turn && state.control.discussing(&state.agent_id) {
            continue;
        }
        if exclusive::check_model(&state.model_settings(), &turn_model).is_err() {
            state.active_model = None;
            turn_model = state.model()?;
            state
                .control
                .scheduler
                .retarget(&state.agent_id, scheduler::Target::model(&turn_model));
            continue;
        }
        state.boundary_notices()?;
        state.model_reminder(&turn_model)?;
        state.control.scope_turn_serviced(&state.agent_id);
        state.drain_mailbox()?;
        let mut provider = state
            .store
            .snapshot()
            .providers
            .get(&turn_model.provider_id)
            .ok_or_else(|| EngineError::bad("active provider was removed"))?
            .clone();
        provider.prompt_caching = turn_model.provider.prompt_caching.clone();
        let mut config = turn_model.config.clone();
        let steering = state
            .context_state
            .frozen
            .as_ref()
            .and_then(|c| c.model_steering.get(&turn_model.id()));
        config.model_prompt = steering.and_then(|s| s.prompt.clone());
        config.model_reminder = steering.and_then(|s| s.reminder.clone());
        config.model_reminder_turns = steering.map(|s| s.reminder_turns);
        config.kind = Some(turn_model.provider.kind);
        config.reasoning.default = turn_model.effort.clone();
        let model = model::Model::new(
            turn_model.provider_id.clone(),
            turn_model.model_id.clone(),
            provider,
            config,
            None,
        )?;
        let requested_focus = state.compact_request.lock().unwrap().take();
        if let Some(focus) = requested_focus {
            compact(state, focus, tx, cancel).await?;
        }
        definitions = state.definitions();
        let mut request = state.prepare(&model, &definitions, &state.history())?;
        if state.agent_id != "root" && !model.command_latest() {
            let threshold = state
                .model_settings()
                .compact_at
                .get(&format!("{}/{}", model.provider_id, model.model_id))
                .copied()
                .or(model.config.compact_at)
                .unwrap_or(0.8);
            let used = state.context_used(request.input_estimate).0;
            let ratio = model
                .config
                .context_window
                .map(|w| used as f64 / w.max(1) as f64)
                .unwrap_or(0.0);
            let budget_full = model.config.context_window.is_some_and(|w| {
                used.saturating_add(model.config.max_output_tokens.unwrap_or(8192)) >= w
            });
            if (ratio >= threshold || budget_full)
                && state.compacted_estimate.is_none_or(|previous| {
                    request.input_estimate
                        > previous.saturating_add(
                            model
                                .config
                                .context_window
                                .unwrap_or(5120)
                                .saturating_div(20)
                                .max(256),
                        )
                })
            {
                compact(state, None, tx, cancel).await?;
                request = state.prepare(&model, &definitions, &state.history())?;
            }
        }
        if state.budget_notice(&request, &model)? {
            request = state.prepare(&model, &definitions, &state.history())?;
        }
        state.meter(&request, &model)?;
        if state.suggest_root_compaction(&model)? {
            request = state.prepare(&model, &definitions, &state.history())?;
            state.meter(&request, &model)?;
        }
        if final_turn {
            request.disable_tools()?;
        }
        request.fallback_ready = fallback_refs.iter().any(|id| {
            !fallback_tried.contains(id) && state.control.model_allowed(&state.agent_id, id)
        });
        {
            let history = state.history();
            let checkpoint = model::ChildRequest {
                provider_id: model.provider_id.clone(),
                primary_config: Some(primary_model.config.clone()),
                primary_effort: primary_model.effort.clone(),
                primary_caching: Some(primary_model.provider.prompt_caching.clone()),
                request: request.clone(),
                system: state.prompt(),
                compacted_estimate: state.compacted_estimate,
                warned_context: state.warned_context,
                config: model.config.clone(),
                effort: model.effort.clone(),
                caching: model.provider.prompt_caching.clone(),
                history_len: history.len(),
                history_hash: model::history_hash(&history),
                context: state.control.journal.lock().unwrap().contexts[&state.agent_id],
            };
            state.control.commit(
                &state.agent_id,
                HistoryEvent::AgentRequest {
                    checkpoint: Box::new(checkpoint.clone()),
                },
            )?;
            if state.agent_id != "root" {
                state.child_profile = Some(checkpoint);
            } else {
                *state.control.last_root_request.lock().unwrap() =
                    Some((model.clone(), request.clone()));
            }
        }
        state.continuing = false;
        let mut completion = match state.stream_model(&model, &request, tx, cancel).await {
            Ok(c) => c,
            Err(error)
                if !cancel.is_cancelled()
                    && matches!(
                        error.kind,
                        ErrorKind::Network | ErrorKind::Unavailable | ErrorKind::Billing
                    ) =>
            {
                if error.kind == ErrorKind::Billing {
                    billing_providers.insert(model.provider_id.clone());
                }
                let mut selected = None;
                for id in &fallback_refs {
                    if !state.control.model_allowed(&state.agent_id, id) {
                        continue;
                    }
                    if id
                        .split_once('/')
                        .is_some_and(|(p, _)| billing_providers.contains(p))
                    {
                        fallback_tried.insert(id.clone());
                        continue;
                    }
                    if fallback_tried.contains(id) {
                        continue;
                    }
                    if error.kind == ErrorKind::Billing
                        && id
                            .split_once('/')
                            .is_some_and(|(p, _)| p == model.provider_id)
                    {
                        continue;
                    }
                    fallback_tried.insert(id.clone());
                    let mut settings = state.model_settings();
                    settings.model = Some(id.clone());
                    settings.effort = None;
                    let Ok(candidate) = build_model(state.store.snapshot(), &settings) else {
                        continue;
                    };
                    if exclusive::check_model(&settings, &candidate).is_err()
                        || candidate.id() == model.id()
                    {
                        continue;
                    }
                    if !candidate
                        .config
                        .capabilities
                        .output
                        .contains(&code_config::Modality::Text)
                    {
                        continue;
                    }
                    let history = state.history();
                    let assets = state.control.journal.lock().unwrap().dir.join("assets");
                    if attachments::validate(
                        &candidate.config.capabilities,
                        &history,
                        Some(&assets),
                    )
                    .is_err()
                    {
                        continue;
                    }
                    state.freeze_tool_model(&candidate)?;
                    let defs = state
                        .context_state
                        .frozen
                        .as_ref()
                        .and_then(|c| c.model_tools.get(id))
                        .cloned()
                        .unwrap_or_else(|| state.definitions());
                    let Ok(preview) = state.prepare(&candidate, &defs, &history) else {
                        continue;
                    };
                    if candidate.config.context_window.is_some_and(|w| {
                        preview
                            .input_estimate
                            .saturating_add(candidate.config.max_output_tokens.unwrap_or(8192))
                            >= w
                    }) {
                        continue;
                    }
                    selected = Some(candidate);
                    break;
                }
                let Some(next) = selected else {
                    // No usable backup remains: pure connectivity loss still reconnects.
                    if error.kind == ErrorKind::Network && request.fallback_ready {
                        fallback_tried.extend(fallback_refs.iter().cloned());
                        continue;
                    }
                    return Err(error);
                };
                let from = model.id();
                let to = next.id();
                let reason = error.message;
                state.control.commit(
                    &state.agent_id,
                    HistoryEvent::ModelSwitch {
                        from: from.clone(),
                        to: to.clone(),
                        reason: reason.clone(),
                    },
                )?;
                state.control.emit_for(
                    &state.agent_id,
                    EngineEvent::ModelSwitched { from, to, reason },
                );
                state
                    .control
                    .emit_for(&state.agent_id, EngineEvent::StreamReset { attempt: 0 });
                state.active_model = Some(next.clone());
                turn_model = next;
                state.anchor = None;
                state.continuing = false;
                continue;
            }
            Err(error) => return Err(error),
        };
        if completion.stop_reason == "interrupted" {
            if !completion.text.is_empty() {
                state.control.commit(
                    &state.agent_id,
                    HistoryEvent::Assistant {
                        text: completion.text.clone(),
                        reasoning: vec![],
                        tool_calls: vec![],
                        replay: None,
                        status: CompletionStatus::Cancelled,
                        native_reason: "interrupted".into(),
                    },
                )?;
            }
            state.control.emit_for(
                &state.agent_id,
                EngineEvent::PartialCompletion {
                    status: CompletionStatus::Cancelled,
                    native_reason: "interrupted".into(),
                    text: completion.text,
                },
            );
            continue;
        }
        state
            .control
            .journal
            .lock()
            .unwrap()
            .activities
            .entry(state.agent_id.clone())
            .and_modify(|a| a.1 = Some(completion.call_id.clone()));
        if state.agent_id == "root" {
            state.control.journal.lock().unwrap().attempt_id = Some(completion.call_id.clone());
        }
        if let Some(input) = completion
            .tokens
            .input_tokens
            .filter(|_| !model.command_latest())
        {
            state.anchor = Some((input, request.input_estimate));
        }
        if model.provider.kind == code_config::ProviderType::Command
            && completion.status == CompletionStatus::Cancelled
        {
            state.control.commit(
                &state.agent_id,
                HistoryEvent::Assistant {
                    text: completion.text.clone(),
                    reasoning: completion.reasoning,
                    tool_calls: vec![],
                    replay: None,
                    status: CompletionStatus::Cancelled,
                    native_reason: "cancelled".into(),
                },
            )?;
            state.control.emit_for(
                &state.agent_id,
                EngineEvent::PartialCompletion {
                    status: CompletionStatus::Cancelled,
                    native_reason: "cancelled".into(),
                    text: completion.text,
                },
            );
            return Err(EngineError::cancelled());
        }
        // A closed malformed call still gets an error result. Repair shape before
        // examining arguments; stream closure is independent of JSON validity.
        for call in &mut completion.tool_calls {
            let _ = repair::repair(call, &request.offered_tools);
        }
        if matches!(
            completion.status,
            CompletionStatus::OutputLimit | CompletionStatus::Paused
        ) {
            let had_calls = !completion.tool_calls.is_empty();
            let paused = completion.status == CompletionStatus::Paused;
            state.control.commit(
                &state.agent_id,
                HistoryEvent::Assistant {
                    text: completion.text.clone(),
                    reasoning: completion.reasoning,
                    tool_calls: vec![],
                    replay: if paused && !had_calls {
                        completion.replay
                    } else {
                        None
                    },
                    status: completion.status.clone(),
                    native_reason: completion.stop_reason.clone(),
                },
            )?;
            state.control.emit_for(
                &state.agent_id,
                EngineEvent::PartialCompletion {
                    status: completion.status,
                    native_reason: completion.stop_reason,
                    text: completion.text,
                },
            );
            let notice = if paused {
                "The provider paused this turn. Continue from where you stopped."
            } else if had_calls {
                "Your output was truncated inside a tool call. No tools from that reply were executed. Re-issue the call with smaller arguments; split large writes into smaller edits."
            } else {
                "Your output was truncated. The partial text is saved. Continue from where you stopped without repeating it."
            };
            state.control.commit(
                &state.agent_id,
                HistoryEvent::SystemReminder {
                    text: notice.into(),
                },
            )?;
            continue;
        }
        if !completion.status.executable() && !completion.tool_calls.is_empty() {
            state.control.emit_for(
                &state.agent_id,
                EngineEvent::PartialCompletion {
                    status: completion.status.clone(),
                    native_reason: completion.stop_reason.clone(),
                    text: completion.text,
                },
            );
            return Err(EngineError::bad(
                "non-executable reply contained tool calls; no calls committed or executed",
            ));
        }
        if !completion.status.executable()
            && !matches!(
                completion.status,
                CompletionStatus::Refusal | CompletionStatus::Filtered | CompletionStatus::Paused
            )
        {
            state.control.emit_for(
                &state.agent_id,
                EngineEvent::PartialCompletion {
                    status: completion.status.clone(),
                    native_reason: completion.stop_reason.clone(),
                    text: completion.text,
                },
            );
            return Err(EngineError::bad(format!(
                "model completion {} ({}); no tool calls executed or assistant history committed",
                completion.status.as_str(),
                completion.stop_reason
            )));
        }
        if !final_turn
            && completion.tool_calls.is_empty()
            && model.config.text_tool_recovery
            && completion.status.executable()
            && let Some(calls) = text_recovery::recover(&completion.text, &request.offered_tools)
        {
            completion.tool_calls.extend(calls);
            completion.replay = None;
        }
        let reasoning = completion
            .reasoning
            .iter()
            .map(|r| r.text.as_str())
            .collect::<String>();
        if let Some(text) = text_recovery::rescue(
            &completion.text,
            &reasoning,
            completion.tool_calls.is_empty(),
        ) {
            completion.text = text.clone();
            completion.replay = None;
            let _ = tx.send(EngineEvent::Text { delta: text });
        }
        for call in &mut completion.tool_calls {
            let _ = repair::repair(call, &request.offered_tools);
        }
        let mut calls = completion.tool_calls.clone();
        if state.control.flat_mode() {
            let names = state.tool_names();
            let tasks = calls
                .iter()
                .filter(|call| names.canonical(&call.name) == "task")
                .count();
            if tasks > 1 {
                // A flat response declares one independent task batch. Start it
                // at the first task, including tasks separated by other calls.
                let first = calls
                    .iter()
                    .position(|call| names.canonical(&call.name) == "task")
                    .unwrap();
                let (batch, rest): (Vec<_>, Vec<_>) = calls
                    .drain(first..)
                    .partition(|call| names.canonical(&call.name) == "task");
                calls.extend(batch);
                calls.extend(rest);
            }
        }
        let assistant_index = state
            .history()
            .iter()
            .filter(|e| matches!(e, HistoryEvent::Assistant { .. }))
            .count();
        let thinking = prior_thinking::substitute(
            state.control.clone(),
            state.store.snapshot().clone(),
            model.config.clone(),
            state.agent_id.clone(),
            assistant_index,
            completion.tool_calls.first().map(|c| c.id.clone()),
            completion.reasoning.clone(),
            cancel.clone(),
        );
        let empty_final = completion.text.trim().is_empty();
        let status = completion.status.clone();
        state.control.commit(
            &state.agent_id,
            HistoryEvent::Assistant {
                text: completion.text,
                reasoning: completion.reasoning.clone(),
                tool_calls: completion.tool_calls,
                replay: completion.replay,
                status: status.clone(),
                native_reason: completion.stop_reason,
            },
        )?;
        for block in completion.reasoning {
            state
                .control
                .commit(&state.agent_id, HistoryEvent::Reasoning { block })?;
        }
        for call in &calls {
            state.control.commit(
                &state.agent_id,
                HistoryEvent::ToolCall { call: call.clone() },
            )?;
        }
        if final_turn {
            thinking.await?;
            for call in &calls {
                state.control.commit(
                    &state.agent_id,
                    HistoryEvent::ToolResult {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        output:
                            "Tools are disabled during the final report turn; no call was executed."
                                .into(),
                        is_error: true,
                        details: Default::default(),
                    },
                )?;
            }
            return Ok("finalized".into());
        }
        if calls.is_empty() {
            thinking.await?;
            if state.control.is_finalizing(&state.agent_id) {
                continue;
            }
            if state.agent_id != "root"
                && status.executable()
                && empty_final
                && !empty_report_warned
            {
                empty_report_warned = true;
                let facts = tools::cap(&progress::facts(&state.control, &state.agent_id));
                state.control.commit(&state.agent_id, HistoryEvent::SystemReminder { text: format!("Your final report was empty. Provide a concise report to your parent: completed work, evidence, remaining work and blockers. If nothing was completed, say why. Do not repeat completed actions. Journal facts:\n{facts}") })?;
                continue;
            }
            let contract = state.control.contract(&state.agent_id);
            if state.agent_id != "root"
                && !criteria_warned
                && contract
                    .completion_criteria()
                    .any(|c| c.owner != "self" && c.status == "open" && c.override_note.is_none())
            {
                criteria_warned = true;
                state.control.commit(&state.agent_id,HistoryEvent::SystemReminder{text:"Before finishing: owned acceptance/findings criteria remain unmet. Record evidence with criteria.met, propose needed changes, or raise criteria.roadblock. If you finish now, your report will mark those criteria unmet.".into()})?;
                continue;
            }
            let running = state.control.running_owned(&state.agent_id);
            if state.agent_id != "root" && !running.is_empty() && !finish_warned {
                finish_warned = true;
                state.control.commit(&state.agent_id, HistoryEvent::SystemReminder { text: format!("Before finishing: you own running background jobs {}. Wait with jobs wait, kill them, or finish and they will be killed with you.", serde_json::to_string(&running).unwrap()) })?;
                continue;
            }
            if state.control.has_mail(&state.agent_id)
                || state.control.has_queued(&state.agent_id, false)
            {
                continue;
            }
            return Ok(status.as_str().into());
        }
        if !status.executable() {
            return Err(EngineError::bad(
                "non-executable completion contained tool calls",
            ));
        }
        let tool_execution = async {
            let names = state.tool_names();
            let foreground_tasks = calls
                .iter()
                .filter(|call| {
                    names.canonical(&call.name) == "task" && call.arguments["background"] != true
                })
                .map(|call| call.id.clone())
                .collect::<Vec<_>>();
            let task_hint_id = (foreground_tasks.len() > 1 && !state.control.flat_mode())
                .then(|| foreground_tasks.last().unwrap());
            let mut index = 0;
            while index < calls.len() {
                let start = index;
                let names = state.tool_names();
                let parallel = |call: &ToolCall| {
                    let canonical = ToolCall {
                        name: names.canonical(&call.name).into(),
                        ..call.clone()
                    };
                    ToolBox::parallel(&canonical)
                        || tree::parallel(&canonical)
                        || (state.control.flat_mode() && canonical.name == "task")
                };
                if parallel(&calls[index]) {
                    while index < calls.len() && parallel(&calls[index]) {
                        index += 1;
                    }
                } else {
                    index += 1;
                }
                let mut pending = vec![];
                for call in &mut calls[start..index] {
                    let failure = state.control.is_finalizing(&state.agent_id)
                        .then(|| "Subtree finalization requested; no new tool calls may start".into())
                        .or_else(|| repair::repair(call, &request.offered_tools).err())
                        .or_else(|| (!call.complete).then(|| "Tool call was not closed by the provider. Re-issue it with complete, smaller arguments.".into()))
                        .or_else(|| guard.check(call));
                    if failure
                        .as_ref()
                        .is_some_and(|s| s.starts_with("Loop blocked:"))
                    {
                        let mut history = state.history();
                        if loop_guard::collapse(&mut history, call) {
                            state.control.commit(
                                &state.agent_id,
                                HistoryEvent::HistoryReplace {
                                    history,
                                    reason: "collapse contiguous rejected identical calls".into(),
                                },
                            )?;
                        }
                    }
                    let _ = tx.send(EngineEvent::ToolStart { call: call.clone() });
                    pending.push((call.clone(), failure));
                }
                let state_ref = &*state;
                let futures = stream::iter(pending.into_iter().map(|(call, failure)| {
                    let cancel = cancel.clone();
                    let barrier = barrier.clone();
                    async move {
                        let finalizing = state_ref.control.is_finalizing(&state_ref.agent_id);
                        let failure = failure.or_else(|| {
                            finalizing.then(|| {
                                "Subtree finalization requested; this queued call was not started".into()
                            })
                        });
                        if let Some(output) = failure {
                            return ToolOutput {
                                details: Default::default(),
                                canonical_call: call,
                                output: if finalizing {
                                    "Tool call refused because this agent is finalizing. Write your final report without tools.".into()
                                } else {
                                    format!("Tool call failed: {output}. Correct the arguments and try again.")
                                },
                                is_error: true,
                            };
                        }
                        execute(state_ref, call, cancel, barrier, tx).await
                    }
                }))
                .buffered(4);
                tokio::pin!(futures);
                let mut completed = start;
                while let Some(mut result) = futures.next().await {
                    completed += 1;
                    if task_hint_id.is_some_and(|id| *id == result.canonical_call.id) {
                        result.output.push('\n');
                        result.output.push_str(&injection::notice(format!(
                            "This response issued {} task calls with background omitted or false. For independent parallel children, set background:true on every task start or continuation, start them all, then collect their jobIds with jobs wait. Do not reissue completed tasks just to add the flag.",
                            foreground_tasks.len()
                        )));
                    }
                    if completed == calls.len() {
                        result
                            .output
                            .push_str(&state.control.context_tag(&state.agent_id, &result.output));
                    }
                    let mut canonical = result.canonical_call.clone();
                    state.control.commit(
                        &state.agent_id,
                        HistoryEvent::CallUpdate {
                            call: canonical.clone(),
                            reason: "prepared/repaired canonical arguments".into(),
                        },
                    )?;
                    if !result.is_error
                        && matches!(
                            state.tool_names().canonical(&canonical.name),
                            "write" | "edit"
                        )
                    {
                        for key in ["content", "old_string", "new_string"] {
                            if let Some(value) = canonical.arguments.get_mut(key)
                                && let Some(s) = value.as_str()
                                && s.len() > 1024
                            {
                                *value = serde_json::json!(format!(
                                    "[{} bytes elided after successful {}]",
                                    s.len(),
                                    canonical.name
                                ));
                            }
                        }
                        state.control.commit(
                            &state.agent_id,
                            HistoryEvent::CallUpdate {
                                call: canonical,
                                reason: "successful write/edit argument elision".into(),
                            },
                        )?;
                    }
                    let call = result.canonical_call;
                    state.control.commit(
                        &state.agent_id,
                        HistoryEvent::ToolResult {
                            details: result.details.clone(),
                            id: call.id.clone(),
                            name: call.name.clone(),
                            output: result.output.clone(),
                            is_error: result.is_error,
                        },
                    )?;
                    let _ = tx.send(EngineEvent::ToolEnd {
                        details: result.details,
                        id: call.id,
                        name: call.name,
                        output: result.output,
                        is_error: result.is_error,
                    });
                }
            }
            Ok::<(), EngineError>(())
        };
        let (thinking_result, tool_result) = tokio::join!(thinking, tool_execution);
        thinking_result?;
        tool_result?;
        if cancel.is_cancelled() {
            return Err(EngineError::cancelled());
        }
        if guard.exhausted() {
            return Err(EngineError::bad("loop guard: repeated identical calls"));
        }
    }
}
fn execute<'a>(
    state: &'a State,
    call: ToolCall,
    cancel: CancellationToken,
    barrier: Arc<CommitBarrier>,
    tx: &'a EventSender,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutput> + Send + 'a>> {
    Box::pin(async move {
        let names = state.tool_names();
        let mut call = call;
        call.name = names.canonical(&call.name).to_owned();
        let progress = goal_controller::mutating_call(&call, &state.cwd);
        let started = std::time::Instant::now();
        let mut output = if state.script_depth == 0
            && matches!(call.name.as_str(), "bash" | "task" | "script")
            && call.arguments["background"] != true
            && !(call.name == "bash"
                && (call.arguments["tty"] == true || call.arguments.get("yieldMs").is_some()))
        {
            foreground::execute(state, call, cancel, barrier).await
        } else {
            execute_raw(state, call, cancel, barrier, tx).await
        };
        if output.canonical_call.name == "script" {
            for event in state.history() {
                if let HistoryEvent::ScriptCall {
                    parent_call_id: Some(parent),
                    result: Some(result),
                    ..
                } = event
                    && parent == output.canonical_call.id
                    && !result.is_error
                {
                    output.details.media.extend(result.details.media);
                }
            }
        }
        output.details.duration_ms =
            Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        output.canonical_call.name = names.configured(&output.canonical_call.name).to_owned();
        if progress && !output.is_error {
            state.control.goal_progress.fetch_add(1, Ordering::Relaxed);
        }
        output
    })
}
fn execute_raw<'a>(
    state: &'a State,
    call: ToolCall,
    cancel: CancellationToken,
    barrier: Arc<CommitBarrier>,
    tx: &'a EventSender,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutput> + Send + 'a>> {
    Box::pin(async move {
        if let Err(error) = state.control.watch_reviewer_guidance(&state.cwd) {
            return ToolOutput {
                details: Default::default(),
                output: error.message,
                is_error: true,
                canonical_call: call,
            };
        }
        let check = matches!(
            call.name.as_str(),
            "bash" | "script" | "write" | "edit" | "generate"
        );
        let id = call.id.clone();
        let writing = matches!(call.name.as_str(), "write" | "edit");
        let nested_script = call.name == "script" && state.script_depth > 0;
        let mut result = execute_inner(state, call, cancel.clone(), barrier, tx).await;
        if writing {
            guidance_selector::after_write(state, &mut result, &cancel).await;
        }
        if nested_script
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&result.output)
        {
            let notices = value["guidance"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            state.guidance_notices.lock().unwrap().extend(notices);
        }
        if check {
            let running = serde_json::from_str::<serde_json::Value>(&result.output)
                .ok()
                .is_some_and(|v| v["status"] == "running" && v["jobId"].is_string());
            let flag = state
                .control
                .check_reviewer_guidance(Some(&id), None, !running);
            // Keep Monty host results valid JSON, while carrying the flag to the model.
            if !flag.is_empty() {
                if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&result.output)
                    && let Some(object) = value.as_object_mut()
                {
                    object.insert("reviewerGuidanceTamper".into(), flag.trim().into());
                    result.output = value.to_string();
                } else {
                    result.output.push_str(&flag);
                }
            }
        }
        result
    })
}
fn execute_inner<'a>(
    state: &'a State,
    call: ToolCall,
    cancel: CancellationToken,
    barrier: Arc<CommitBarrier>,
    tx: &'a EventSender,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutput> + Send + 'a>> {
    Box::pin(async move {
        let mut call = call;
        if let Err(error) = state.control.normalize_ids(&mut call) {
            return ToolOutput {
                details: Default::default(),
                output: format!("Error: {error}"),
                is_error: true,
                canonical_call: call,
            };
        }
        if state.control.flat_mode()
            && state.agent_id != "root"
            && matches!(
                call.name.as_str(),
                "task" | "agents" | "message" | "answer" | "escalate" | "models" | "report"
            )
        {
            return ToolOutput {
                details: Default::default(),
                output: "Tree controls and delegation are unavailable to flat-mode children."
                    .into(),
                is_error: true,
                canonical_call: call,
            };
        }
        if matches!(call.name.as_str(), "goals" | "todo") {
            return ToolOutput { details: Default::default(),
                output: "This tool is retired. Use criteria and steps through Monty: criteria.show(), criteria.add(kind='step', text=...), criteria.step(id=..., status='met').".into(),
                is_error: true,
                canonical_call: call,
            };
        }
        let drafting = state.agent_id == "root"
            && state
                .control
                .goal_state()
                .is_some_and(|g| g.phase == "drafting");
        let draft_allowed = (matches!(
            call.name.as_str(),
            "read"
                | "read_image"
                | "read_video"
                | "grep"
                | "glob"
                | "history"
                | "models"
                | "script"
                | "skill"
        ) && call.arguments["background"] != true)
            || (call.name == "criteria"
                && matches!(
                    call.arguments["action"].as_str(),
                    Some("show" | "query" | "set_objective" | "add" | "step")
                ));
        let draft_allowed = draft_allowed
            || (call.name == "agents"
                && matches!(
                    call.arguments["action"].as_str(),
                    Some("list" | "show" | "ask")
                ));
        let draft_allowed = draft_allowed || call.name == "decisions";
        if drafting && !draft_allowed {
            return ToolOutput { details: Default::default(),output:"Goal drafting is read-only. Allowed: read, grep, glob, history, models, criteria draft changes, and script containing those functions.".into(),is_error:true,canonical_call:call};
        }
        if state
            .control
            .contract(&state.agent_id)
            .roadblock()
            .is_some()
            && !(call.name == "criteria"
                && matches!(call.arguments["action"].as_str(), Some("show" | "query")))
        {
            return ToolOutput{ details: Default::default(),output:"Contract is roadblocked; stop and wait for the owner to pivot, stop or override.".into(),is_error:true,canonical_call:call};
        }
        if tools::media::handles(&call.name) {
            return tools::media::execute(state, call, cancel, barrier).await;
        }
        let mut variation_note = None;
        let result:Result<String,EngineError>=async{
        state.control.check_owner()?;
        if tools::web::handles(&call.name) {
            let blocked = state.model_settings().exclusive.as_ref().is_some_and(|ex| ex.block_network);
            let web = state.config.lock().unwrap().config.web.clone();
            return tools::web::execute(state.gate.as_ref(), state.metadata_operation(call.clone(), None), &web, blocked, &cancel)
                .await
                .map_err(|e| EngineError::bad(format!("{e:#}")));
        }
        if call.name == "generate" {
            let (session_id, assets) = { let j = state.control.journal.lock().unwrap(); (j.header.id.clone(), j.dir.join("assets")) };
            let snapshot = state.config.lock().unwrap().clone();
            let generated = tools::generate::execute(tools::generate::Context {
                control: &state.control,
                gate: state.gate.as_ref(),
                op: state.metadata_operation(call.clone(), None),
                snapshot,
                session_id,
                agent_id: state.agent_id.clone(),
                assets,
                cancel: &cancel,
            })
            .await
            .map_err(|e| EngineError::bad(format!("{e:#}")))?;
            if let Some(usage) = generated.usage {
                state.control.commit(&state.agent_id, HistoryEvent::Usage { usage: usage.clone() })?;
                state.control.emit(EngineEvent::Usage { usage });
            }
            return Ok(generated.output);
        }
        if call.name == "bash" && call.arguments["background"] == true {
            let operation = state.metadata_operation(call.clone(), None);
            if let Some((revised,note)) = state.gate.revise(&operation,true,&cancel).await.map_err(|e|EngineError::bad(e.to_string()))? {
                call = revised;
                variation_note = Some(note);
            }
        }
        if let Some((target,scope)) = external_metadata(state,&call)? {
            let mut operation = state.metadata_operation(call.clone(),Some(target));
            operation.cwd = scope;
            if let Some((revised,note)) = state.gate.revise(&operation,true,&cancel).await.map_err(|e|EngineError::bad(e.to_string()))? {
                call = revised;
                variation_note = Some(note);
                let resolved = external_metadata(state,&call)?;
                let scope = resolved.as_ref().map(|(_,scope)|scope.clone()).unwrap_or_else(||if call.name == "task" {state.control.journal.lock().unwrap().header.cwd.clone()} else {state.cwd.clone()});
                operation = state.metadata_operation(call.clone(),resolved.map(|(target,_)|target));
                operation.cwd = scope;
            }
            state.gate.authorize(&operation,&cancel).await.map_err(|e|EngineError::bad(e.to_string()))?;
            if let Some(path) = &operation.target {
                paths::Resolver::new(state.cwd.clone()).validate(path).map_err(|e|EngineError::bad(e.to_string()))?;
            }
        }
        if let Some(foreground) = &state.tools.foreground {
            let mut canonical = foreground.canonical.lock().unwrap();
            if canonical.as_ref().is_some_and(|current| current.id == call.id) { *canonical = Some(call.clone()); }
        }
        match call.name.as_str(){
            "terminal" => user_terminal::execute(state, &call, &cancel).await,
            "skill" => skill_commands::read(state, &call.arguments, &cancel, barrier.clone(), tx).await,
            "script" => script::execute(state, &call, &cancel, barrier.clone(), tx).await,
            "history"=>{
                let target={let j=state.control.journal.lock().unwrap();call.arguments["session"].as_str().filter(|id|*id!="current"&&*id!=j.header.id).map(|id|j.dir.parent().unwrap().join(id))};
                let resolver=paths::Resolver::new(state.cwd.clone());
                let target=target.map(|path|resolver.resolve(&path.to_string_lossy())).transpose().map_err(|e|EngineError::bad(e.to_string()))?;
                if let Some(target)=&target {resolver.validate(target).map_err(|e|EngineError::bad(e.to_string()))?;}
                session_tools::history(&state.control.journal.lock().unwrap(),&state.agent_id,&call.arguments,target.as_deref()).map_err(|e|EngineError::bad(e.to_string()))
            }
            "criteria" => goal::execute(state,&call.arguments,&cancel,barrier.clone(),tx).await,
            "question" => {
                let questions = questions::parse(&call.arguments)?;
                let blocking = match call.arguments.get("wait") {
                    Some(serde_json::Value::String(wait)) if wait == "blocking" => true,
                    Some(serde_json::Value::String(wait)) if wait == "whenever" => false,
                    None => {
                        let mode = state.control.journal.lock().unwrap().settings.mode
                            .unwrap_or_else(|| state.config.lock().unwrap().config.permissions.mode);
                        mode == PermissionMode::Manual
                    },
                    _ => return Err(EngineError::bad("wait must be blocking or whenever")),
                };
                let id = state.control.ask_questions(&state.agent_id, Some(call.id.clone()), questions, blocking)?;
                if blocking {
                    let answer = state.control.wait_question(&state.agent_id, &id, &cancel).await?;
                    let answers = state.control.journal.lock().unwrap().questions.get(&id).and_then(|q|q.answers.clone());
                    Ok(serde_json::json!({"questionId":id,"callId":call.id,"answer":answer,"answers":answers}).to_string())
                } else {
                    Ok(serde_json::json!({"questionId":id,"callId":call.id,"status":"pending; answer arrives at next model boundary"}).to_string())
                }
            },
            _=>tree::execute(state, &call, &cancel, barrier.clone(), tx).await,

        }
    }.await.map(|output|if let Some(note)=variation_note {format!("{note}\n{output}")} else {output});
        if !tree::handles(&call)
            && !matches!(
                call.name.as_str(),
                "history"
                    | "criteria"
                    | "skill"
                    | "question"
                    | "terminal"
                    | "script"
                    | "web_search"
                    | "web_fetch"
                    | "generate"
            )
        {
            return state.tools.execute(call, cancel, barrier, tx).await;
        }
        match result {
            Ok(output) => ToolOutput {
                details: Default::default(),
                output: if state.script_depth > 0
                    || (call.name == "script"
                        && serde_json::from_str::<serde_json::Value>(&output)
                            .is_ok_and(|v| v["guidance"].is_array()))
                {
                    output
                } else {
                    state
                        .control
                        .retain_text(&call.id, &output)
                        .unwrap_or_else(|e| {
                            format!("{}\nArtifact retention failed: {e}", tools::cap(&output))
                        })
                },
                is_error: false,
                canonical_call: call,
            },
            Err(e) => ToolOutput {
                details: Default::default(),
                output: format!("Error: {e}"),
                is_error: true,
                canonical_call: call,
            },
        }
    })
}
fn external_metadata(
    state: &State,
    call: &ToolCall,
) -> Result<Option<(PathBuf, PathBuf)>, EngineError> {
    let resolver = paths::Resolver::new(state.cwd.clone());
    let raw = if call.name == "history" {
        let j = state.control.journal.lock().unwrap();
        call.arguments["session"]
            .as_str()
            .filter(|id| *id != "current" && *id != j.header.id)
            .map(|id| {
                j.dir
                    .parent()
                    .unwrap()
                    .join(id)
                    .to_string_lossy()
                    .into_owned()
            })
    } else if call.name == "task" && call.arguments.get("continue").is_none() {
        Some(call.arguments["cwd"].as_str().unwrap_or(".").to_owned())
    } else {
        None
    };
    let Some(raw) = raw else {
        return Ok(None);
    };
    let target = resolver
        .resolve(&raw)
        .map_err(|e| EngineError::bad(e.to_string()))?;
    let scope = if call.name == "task" {
        state.control.journal.lock().unwrap().header.cwd.clone()
    } else {
        state.cwd.clone()
    };
    if call.name == "task" && target.starts_with(&scope) {
        return Ok(None);
    }
    Ok(Some((target, scope)))
}
async fn compact(
    state: &mut State,
    focus: Option<String>,
    tx: &EventSender,
    cancel: &CancellationToken,
) -> Result<String, EngineError> {
    state.control.check_owner()?;
    state.reload()?;
    state.await_model(cancel).await?;
    state.pair_interrupted_calls()?;
    state.drain_mailbox()?;
    let mut model = state.model()?;
    let window = model.config.context_window.unwrap_or(65536);
    model.config.max_output_tokens = Some(
        model
            .config
            .max_output_tokens
            .unwrap_or(8192)
            .min((window / 8).max(256)),
    );

    let history = state.history();
    let latest = history.iter().rposition(|e| match e {
        HistoryEvent::User {..} => true,
        HistoryEvent::SystemReminder {text} => injection::body(text).unwrap_or(text).starts_with("Latest unanswered"),
        HistoryEvent::Message {to,..} => to == &state.agent_id,
        _ => false,
    }).and_then(|i| {
        let answered = history[i+1..].iter().any(|e| matches!(e,HistoryEvent::Assistant {tool_calls,status,..} if tool_calls.is_empty() && matches!(status,CompletionStatus::EndTurn|CompletionStatus::Refusal|CompletionStatus::Filtered)));
        if answered { None } else { Some(match &history[i] {
            HistoryEvent::Message {from,text,..} => HistoryEvent::SystemReminder {text:format!("Latest unanswered message from {from} (continue from the summary; do not repeat completed steps): {text}")},
            HistoryEvent::User { attachments, .. } if !attachments.is_empty() => history[i].clone(),
            HistoryEvent::User { text, .. } => HistoryEvent::SystemReminder {text:format!("Latest unanswered user request (continue from the summary; do not repeat completed steps): {text}")},
            HistoryEvent::SystemReminder { text } => HistoryEvent::SystemReminder { text:injection::body(text).unwrap_or(text).to_owned() },
            event => event.clone(),
        }) }
    });
    let tail_start = compaction::tail_start(&history, window / 4);
    let tail = history[tail_start..].to_vec();
    state.control.agent_status(&state.agent_id, "compacting")?;
    let brief = compaction::draft(
        state,
        &model,
        &history[..tail_start],
        focus.as_deref(),
        tx,
        cancel,
    )
    .await?;
    let facts = progress::facts(&state.control, &state.agent_id);
    let git = tokio::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ])
        .current_dir(&state.cwd)
        .output();
    let git = tokio::select! {
        _ = cancel.cancelled() => return Err(EngineError::cancelled()),
        result = tokio::time::timeout(std::time::Duration::from_secs(3), git) => {
            match result {
                Ok(Ok(output)) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    format!("Git: {}; dirty entries: {}", text.lines().next().unwrap_or("unknown branch"), text.lines().skip(1).count())
                }
                _ => "Git: unavailable (not a repository, timeout, or command error)".into(),
            }
        }
    };
    let appendix = format!("Deterministic journal appendix:\n{facts}\n{git}");
    // Keep a bounded appendix and an explicit history pointer if unusually large.
    let appendix_limit = (window / 10).max(128);
    let mut appendix_end = appendix.len();
    while model::count_tokens(&appendix.as_bytes()[..appendix_end]) > appendix_limit {
        appendix_end = appendix_end * 3 / 4;
        while !appendix.is_char_boundary(appendix_end) {
            appendix_end -= 1;
        }
    }
    let appendix = if appendix_end < appendix.len() {
        format!(
            "{}\n[Appendix abbreviated; complete facts remain in earlier context history.]\n{git}",
            &appendix[..appendix_end]
        )
    } else {
        appendix
    };
    let footer = session_tools::earlier_context_footer(
        &state.control.journal.lock().unwrap(),
        &state.agent_id,
    );
    let footer = state.control.tool_names(&state.agent_id).rewrite(&footer);
    let summary = format!("{brief}\n\n{appendix}\n\n{footer}");
    state.freeze_context()?;
    let mut events = vec![
        HistoryEvent::PromptContext {
            context: Box::new(state.context_state.frozen.as_ref().unwrap().clone()),
        },
        HistoryEvent::SystemReminder {
            text: format!(
                "cwd: {}. Continue from this compacted context; the session system prompt is unchanged.",
                state.cwd.display()
            ),
        },
        HistoryEvent::Compaction {
            summary: summary.clone(),
        },
    ];
    let (todos, _) = state.tools.context_observations();
    if todos.as_array().is_some_and(|items| !items.is_empty()) {
        events.push(HistoryEvent::SystemReminder {
            text: format!("Current todo state: {todos}"),
        });
    }
    let contract = state.control.contract(&state.agent_id);
    events.push(HistoryEvent::Goal {
        contract: contract.clone(),
    });
    events.push(HistoryEvent::SystemReminder {
        text: format!(
            "Your current contract (authoritative after compaction): {}",
            serde_json::to_string(&contract).unwrap()
        ),
    });
    events.push(HistoryEvent::SystemReminder {
        text: state.control.decision_context(&state.agent_id),
    });
    events.push(HistoryEvent::SystemReminder {
        text: state.control.background_snapshot(&state.agent_id),
    });
    events.push(HistoryEvent::SystemReminder {
        text: state.control.scope_context_snapshot(&state.agent_id),
    });
    let latest_in_tail =
        latest.is_some() && tail.iter().any(|e| matches!(e, HistoryEvent::User { .. }));
    if !latest_in_tail && let Some(latest) = latest {
        events.push(latest);
    }
    events.extend(tail);
    let active_model = state.model()?;
    let final_request = state.prepare(&active_model, &state.definitions(), &events)?;
    if active_model.config.context_window.is_some_and(|w| {
        final_request
            .input_estimate
            .saturating_add(active_model.config.max_output_tokens.unwrap_or(8192))
            >= w
    }) {
        return Err(EngineError::bad(
            "compacted summary, whole-exchange tail and authoritative criteria do not fit; current context retained",
        ));
    }
    state
        .control
        .commit(&state.agent_id, HistoryEvent::Compaction { summary })?;
    let envelopes = state
        .control
        .journal
        .lock()
        .unwrap()
        .rotate(&state.agent_id, events)?;
    for envelope in envelopes {
        state.control.emit(EngineEvent::Committed { envelope });
    }
    *state.guidance_turn.lock().unwrap() = guidance_selector::Turn::default();
    state.continuing = false;
    state.anchor = None;
    state.warned_context = false;
    state.context_state.budget_bucket = 0;
    state.context_state.external_files.clear();
    // Estimate the actual rotated history (including canonical notice wrappers)
    // with the same frozen prefix used by the next author request.
    let request = state.prepare(&active_model, &state.definitions(), &state.history())?;
    state.compacted_estimate = Some(request.input_estimate);
    state.control.agent_status(&state.agent_id, "running")?;
    state.meter(&request, &active_model)?;
    Ok("compacted".into())
}
