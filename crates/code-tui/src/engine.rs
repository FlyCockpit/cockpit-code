//! Engine adapter: the only module that touches `code_engine` / `code_config`
//! types. Everything else in the TUI speaks the plain types defined here, so
//! engine changes (subagents, jobs, MCP) stay local.
//!
//! Layers, lowest first: `views` (and the feature views in `goal`, `tree`,
//! `terminal`) are the plain data the UI renders; `convert` turns engine
//! records into them, `catalog` the configuration, `events` the event
//! stream and the journal; [`Engine`] here is the handle the UI drives.
use crate::usage::ProviderUsage;
use code_config::{ConfigStore, Directories, PermissionMode};
pub use code_engine::ForkChatMessage;
pub use code_engine::schema::{FileEdit, ToolCall, ToolResultData, ToolResultDetails};
use code_engine::{
    Session, SessionOptions,
    schema::{ApprovalDecision, EngineError, EngineEvent, ErrorKind, EventEnvelope, HistoryEvent},
    sessions::{self, ForkPoint, SessionSettings},
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use tokio::sync::mpsc;

mod catalog;
mod convert;
mod events;
mod goal;
mod terminal;
mod text;
mod tree;
mod views;
pub use catalog::{Catalog, ModelInfo};
use convert::{
    cost_of, guidance_sel, guidance_view, job_view, prune_view, queued_view, scope_view,
    structured_items, totals_view, usage_view, user_wire,
};
use events::{replay_items, subscribe, translate};
pub use goal::{
    ContractDraft, ContractView, CriterionView, DraftItem, GoalView, ProposalView, RoadblockView,
    Section,
};
pub use terminal::{TermCmd, TermInfo, TermMsg, TermRx, TermTx};
use text::strip_ctx;
pub use text::strip_guidance;
pub use tree::{AgentDone, DecisionView, Review, TreeRow, agent_filters};
#[cfg(test)]
pub use views::QOption;
pub use views::{
    AgentView, ApprovalChoice, ApprovalView, ChangeView, EventRx, Exclusive, GuidanceSel,
    GuidanceView, JobView, Launch, LocalRun, LockInfo, Mode, OpenError, PruneView, QAnswer, QItem,
    QKind, QuestionView, QueueClass, QueueOutcome, Queued, Replay, ReplayItem, ScopeChoice,
    ScopeView, SkillView, Target, Totals, UiEvent, Usage, age, alive, answers_text, fmt_cost,
    status_text,
};
use views::{ForkCandidate, McpView, ResumeContext, SessionRow};

fn err(e: EngineError) -> String {
    e.message
}

/// Parse `<session>[@<agent>:<context>:<line>]`, as the headless `--fork`.
fn parse_fork(s: &str) -> Result<(String, Option<(u32, usize)>), String> {
    let Some((id, p)) = s.split_once('@') else {
        return Ok((s.into(), None));
    };
    let parts: Vec<_> = p.split(':').collect();
    if parts.len() != 3 || parts[0] != "root" {
        return Err("fork point must be <session>@root:<context>:<line>".into());
    }
    let context = parts[1].parse().map_err(|_| "bad fork context")?;
    let line = parts[2].parse().map_err(|_| "bad fork line")?;
    Ok((id.into(), Some((context, line))))
}

/// A background discovery run: its providers and the reloaded store.
type Discovery = (
    BTreeSet<String>,
    std_mpsc::Receiver<Result<ConfigStore, String>>,
);

/// Owns the engine session plus a config view for the model catalog.
pub struct Engine {
    directories: Directories,
    store: ConfigStore,
    session: Session,
    cwd: PathBuf,
    /// A mode chosen by flag or carried over by /new, applied at the first
    /// send so an unused session is never written.
    pending_mode: Option<PermissionMode>,
    /// Shutdowns of sessions left by a switch; awaited before exit.
    retired: Vec<tokio::task::JoinHandle<()>>,
    /// `--model` given with a resume/continue/fork: applied by the UI after
    /// the cache-aware confirmation, not at open.
    pub deferred_model: Option<String>,
    /// A background discovery run: its providers and the reloaded store.
    discovery: Option<Discovery>,
    model_selection: Option<std_mpsc::Receiver<Result<String, String>>>,
    /// Opting a switched-to session into private terminals failed.
    pub terminal_issue: Option<String>,
}

impl Engine {
    /// Open the session the flags name. A lock conflict or a missing session
    /// falls back to a fresh session and is reported for the UI to show.
    pub fn start(launch: Launch) -> Result<(Self, EventRx, Option<OpenError>), String> {
        let directories = Directories::resolve().map_err(|e| e.to_string())?;
        let store = ConfigStore::load(directories.clone()).map_err(|e| e.to_string())?;
        let cwd = std::fs::canonicalize(&launch.cwd).unwrap_or(launch.cwd.clone());
        let options = SessionOptions {
            model: launch.model.clone(),
            effort: launch.effort.clone(),
        };
        // An existing conversation switches models through the UI's
        // cache-aware confirmation.
        let has_history =
            launch.fork.is_some() || launch.continue_session || launch.resume.is_some();
        let deferred_model = launch.model.clone().filter(|_| has_history);
        let target_options = SessionOptions {
            model: if has_history {
                None
            } else {
                launch.model.clone()
            },
            effort: launch.effort.clone(),
        };
        let mut issue = None;
        let target = if let Some(f) = &launch.fork {
            let (id, point) = parse_fork(f)?;
            Some(Target::Fork(id, point))
        } else if launch.continue_session {
            match sessions::list_in(&directories.data, Some(&cwd)) {
                Ok(rows) => match rows.into_iter().find(|r| !r.title.is_empty()) {
                    Some(r) if launch.claim_lock => Some(Target::Claim(r.header.id)),
                    Some(r) => Some(Target::Resume(r.header.id)),
                    None => {
                        issue = Some(OpenError::Other("no sessions for this cwd".into()));
                        None
                    }
                },
                Err(e) => return Err(err(e)),
            }
        } else {
            launch.resume.clone().map(|id| {
                if launch.claim_lock {
                    Target::Claim(id)
                } else {
                    Target::Resume(id)
                }
            })
        };
        let opened = match target {
            Some(t) => match open(&directories, &cwd, t, target_options) {
                Ok(s) => Some(s),
                Err(e) => {
                    issue = Some(e);
                    None
                }
            },
            None => None,
        };
        let session = match opened {
            Some(s) => s,
            None => match open(&directories, &cwd, Target::New, options) {
                Ok(s) => s,
                Err(OpenError::Other(e)) => return Err(e),
                Err(OpenError::Locked(_)) => return Err("new session is locked".into()),
            },
        };
        if launch.flat {
            session
                .set_subagents(code_engine::SubagentMode::Flat)
                .map_err(err)?;
        }
        let pending_mode = launch
            .mode
            .as_deref()
            .and_then(Mode::parse)
            .map(Mode::engine);
        // This frontend renders private terminals; opt in before
        // the first turn.
        let terminal_issue = session.enable_user_terminals().err();
        let rx = subscribe(
            &session,
            directories
                .data
                .join("sessions")
                .join(session.id())
                .join("assets"),
        );
        // A failed open falls back to a new session, which already got the model.
        let deferred_model = deferred_model.filter(|_| issue.is_none());

        Ok((
            Self {
                directories,
                store,
                session,
                cwd,
                pending_mode,
                retired: vec![],
                deferred_model,
                discovery: None,
                model_selection: None,
                terminal_issue: terminal_issue
                    .map(|e| format!("private terminals unavailable: {}", e.message)),
            },
            rx,
            issue,
        ))
    }

    /// Switch to another session. The current one must be idle; it is closed
    /// only after the new one opened.
    pub fn switch(&mut self, target: Target) -> Result<EventRx, OpenError> {
        if self.session.is_active() {
            return Err(OpenError::Other("cancel the running turn first".into()));
        }
        let settings = self.session.settings();
        let mode = self.pending_mode.or(settings.mode);
        let new = matches!(target, Target::New);
        let session = open(
            &self.directories,
            &self.cwd,
            target,
            if new {
                SessionOptions {
                    model: settings.model.clone(),
                    effort: settings.effort.clone(),
                }
            } else {
                SessionOptions::default()
            },
        )?;
        // /new keeps the mode without writing the new session yet.
        self.pending_mode = if new { mode } else { None };
        let old = std::mem::replace(&mut self.session, session);
        let terminal_issue = self.enable_terminals().err();
        // The old session's jobs and children stop; its lock is released after.
        self.retired.push(tokio::spawn(async move {
            let _ = old.shutdown().await;
        }));
        let rx = subscribe(&self.session, self.assets_dir());
        if let Some(e) = terminal_issue {
            self.terminal_issue = Some(format!("private terminals unavailable: {e}"));
        }
        Ok(rx)
    }

    /// Stop the whole tree (turn, jobs, children), record final states and
    /// release the lock; also waits for sessions left by a switch.
    pub async fn close(&mut self) {
        let _ = self.session.shutdown().await;
        for handle in self.retired.drain(..) {
            let _ = handle.await;
        }
    }

    pub fn session_id(&self) -> String {
        self.session.id()
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Lock holder of an unopened session, if any.
    pub fn lock_info(&self, id: &str) -> Option<LockInfo> {
        lock_info(&self.directories, id)
    }

    /// Sessions for this cwd (or all), most recent first, plus unreadable
    /// session directories. Sessions without a first message are hidden.
    pub fn list_sessions(&self, all: bool) -> Result<(Vec<SessionRow>, Vec<String>), String> {
        let listing = sessions::list_with_problems_in(
            &self.directories.data,
            if all { None } else { Some(&self.cwd) },
        )
        .map_err(err)?;
        let current = self.session.id();
        let rows = listing
            .sessions
            .into_iter()
            .filter(|r| !r.title.is_empty() && r.header.id != current)
            .map(|r| SessionRow {
                id: r.header.id,
                title: r.title,
                age: age(&r.updated_at),
                model: r.model,
                cost: r.cost,
                cwd: r.header.cwd.display().to_string(),
                locked: r.lock.holder.is_some(),
                stale: r.lock.holder.is_some() && r.lock.pid_alive == Some(false),
            })
            .collect();
        let problems = listing
            .problems
            .into_iter()
            .map(|p| format!("{}: {}", p.path.display(), p.error.message))
            .collect();
        Ok((rows, problems))
    }

    /// Rename another (closed) session with a user title.
    pub fn rename_session(&self, id: &str, title: &str) -> Result<(), String> {
        sessions::rename_session(&self.store, id, title).map_err(err)
    }
    /// Delete another session; refused while it is locked.
    pub fn delete_session(&self, id: &str) -> Result<(), String> {
        if id == self.session.id() {
            return Err("this session is open here".into());
        }
        sessions::delete_session(&self.directories.data, id).map_err(err)
    }
    /// The last messages of another session, for the picker preview.
    pub fn session_preview(&self, id: &str, limit: usize) -> Vec<(String, String)> {
        sessions::recent_messages(&self.directories.data, id, limit)
            .unwrap_or_default()
            .into_iter()
            .map(|(r, t)| (r, strip_ctx(&t)))
            .collect()
    }

    /// Transcript of the open session's root agent from its context files,
    /// plus every recorded usage of the tree.
    pub fn replay(&self) -> Replay {
        self.replay_agent("root", true)
    }

    /// Transcript of one agent from its context files.
    pub fn replay_agent(&self, agent: &str, with_usage: bool) -> Replay {
        // Only the current context and the one before it are read and
        // laid out; older ones stay on disk until scrolled to.
        let mut out = Replay::default();
        if with_usage {
            out.usage = self
                .session
                .usage_records()
                .iter()
                .map(usage_view)
                .collect();
        }
        let mut numbers = self.session.context_numbers(agent);
        let assets = self.assets_dir();
        let read = |n: u32| -> Vec<ReplayItem> {
            self.session
                .read_transcript_context(agent, n)
                .map(|events| replay_items(agent, &events, &assets))
                .unwrap_or_default()
        };
        if let Some(newest) = numbers.pop() {
            out.current = read(newest);
        }
        if let Some(prev) = numbers.pop() {
            let items = read(prev);
            if !items.is_empty() {
                out.earlier.push(items);
            }
        }
        out.older = numbers;
        out
    }

    /// Read one older context in the background (scroll-back).
    pub fn load_context(&self, agent: &str, n: u32) -> std_mpsc::Receiver<Vec<ReplayItem>> {
        let (tx, rx) = std_mpsc::channel();
        let session = self.session.clone();
        let assets = self.assets_dir();
        let agent = agent.to_string();
        std::thread::spawn(move || {
            let items = session
                .read_transcript_context(&agent, n)
                .map(|events| replay_items(&agent, &events, &assets))
                .unwrap_or_default();
            let _ = tx.send(items);
        });
        rx
    }

    /// The newest context file of `agent`, read from disk.
    fn current_context(&self, agent: &str) -> Vec<EventEnvelope> {
        self.session
            .context_numbers(agent)
            .last()
            .and_then(|n| self.session.read_context(agent, *n).ok())
            .unwrap_or_default()
    }

    /// Root user messages from every journal context, including compacted ones.
    pub fn prompt_history(&self) -> Vec<String> {
        let assets = self.assets_dir();
        let mut out = vec![];
        for number in self.session.context_numbers("root") {
            let Ok(events) = self.session.read_context("root", number) else {
                continue;
            };
            // rotate() gives the carried context the compaction turn's identity.
            // Later user turns have different identities. Skip carried copies,
            // while preserving genuinely repeated messages and the original tail.
            let compacted = number > 1
                && events
                    .iter()
                    .take(3)
                    .any(|event| matches!(event.event, HistoryEvent::Compaction { .. }));
            let seed_turn = events.first().and_then(|event| event.turn_id.as_deref());
            for event in &events {
                if event.agent_id != "root" || (compacted && event.turn_id.as_deref() == seed_turn)
                {
                    continue;
                }
                if let HistoryEvent::User { text, attachments } = &event.event {
                    let text = user_wire(text, attachments, &assets);
                    if !text.trim().is_empty() {
                        out.push(text);
                    }
                }
            }
        }
        out
    }

    /// User messages of the root agent, oldest first, for `/fork` at a message.
    pub fn fork_candidates(&self) -> Vec<ForkCandidate> {
        let mut out = vec![];
        for n in self.session.context_numbers("root") {
            let Ok(envelopes) = self.session.read_context("root", n) else {
                continue;
            };
            for (i, e) in envelopes.iter().enumerate() {
                if let HistoryEvent::User { text, .. } = &e.event {
                    // Carried-over messages after a compaction duplicate an earlier one.
                    if out
                        .iter()
                        .any(|c: &ForkCandidate| c.text == strip_ctx(text))
                        && i < 8
                        && n > 1
                    {
                        continue;
                    }
                    out.push(ForkCandidate {
                        text: strip_ctx(text),
                        context: n,
                        line: i,
                    });
                }
            }
        }
        out
    }

    /// Re-read config files if they changed. Errors keep the last valid view.
    pub fn reload(&mut self) -> Option<String> {
        self.store.reload_if_changed().err().map(|e| e.to_string())
    }

    /// Start model discovery in the background (`ConfigStore`): expired
    /// providers and failures due for a retry, or every discovering provider
    /// when `force` (`/model` Ctrl+R). False when nothing needs it or a run
    /// is already going.
    pub fn discover(&mut self, force: bool) -> bool {
        if self.discovery.is_some() {
            return false;
        }
        let targets: BTreeSet<String> = if force {
            self.catalog()
                .providers
                .into_iter()
                .filter(|p| p.discovers && p.enabled)
                .map(|p| p.id)
                .collect()
        } else {
            catalog::discovery_expired(self.store.snapshot(), &self.directories)
        };
        if targets.is_empty() {
            return false;
        }
        let mut store = self.store.clone();
        let list = targets.clone();
        let (tx, rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            // One enrichment pass forces exactly these providers;
            // the others keep their caches.
            let result = store
                .refresh_discovery_many(&list)
                .map_err(|e| e.to_string());
            let _ = tx.send(result.map(|_| store));
        });
        self.discovery = Some((targets, rx));
        true
    }

    /// Where clipboard images are saved before they are sent; the engine
    /// imports every attachment into the session `assets/` at send.
    pub fn attachment_dir(&self) -> PathBuf {
        self.directories.cache.join("attachments")
    }

    /// Providers being discovered now: a `/model` refresh, or the engine's
    /// own background refresh (its expired providers).
    pub fn discovering(&self) -> BTreeSet<String> {
        match &self.discovery {
            Some((s, _)) => s.clone(),
            None if self.session.discovery_running() => {
                catalog::discovery_expired(self.store.snapshot(), &self.directories)
            }
            None => BTreeSet::new(),
        }
    }

    /// A finished discovery run: Some(Ok) after the catalog changed.
    pub fn poll_discovery(&mut self) -> Option<Result<(), String>> {
        let (_, rx) = self.discovery.as_ref()?;
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(std_mpsc::TryRecvError::Empty) => return None,
            Err(std_mpsc::TryRecvError::Disconnected) => Err("discovery stopped".into()),
        };
        self.discovery = None;
        Some(result.map(|store| self.store = store))
    }

    /// Ask the engine's usage cache to re-run every provider's usage
    /// command now; without `force` the cache's TTL decides.
    pub fn refresh_usage(&mut self, force: bool) {
        if force {
            self.session.refresh_usage();
        }
    }

    /// Every enabled provider's usage, from the engine cache (no I/O but
    /// the discovery-account fallback).
    pub fn provider_usage(&self) -> Vec<ProviderUsage> {
        let snap = self.store.snapshot();
        self.session
            .provider_usage()
            .into_iter()
            .filter_map(|u| {
                let p = snap.providers.get(&u.provider).filter(|p| p.enabled)?;
                Some(catalog::usage_of(&self.directories, p, u))
            })
            .collect()
    }

    pub fn catalog(&self) -> Catalog {
        let exclusive = self.session.exclusive().map(|ex| ex.model);
        catalog::build(
            self.store.snapshot(),
            exclusive.as_deref(),
            &self.directories.config,
        )
    }

    fn settings(&self) -> SessionSettings {
        self.session.settings()
    }
    /// The model the next turn will use.
    pub fn active_model(&self) -> Option<String> {
        self.settings()
            .model
            .or_else(|| self.store.snapshot().config.default_model.clone())
    }
    pub fn effort(&self) -> Option<String> {
        self.settings().effort
    }
    pub fn set_model(&mut self, model: String) -> Result<(), String> {
        if self.model_selection.is_some() {
            return Err("model selection is waiting for provider discovery".into());
        }
        let session = self.session.clone();
        let (tx, rx) = std_mpsc::channel();
        tokio::spawn(async move {
            let result = session
                .set_model_ready(model.clone())
                .await
                .map(|_| model)
                .map_err(err);
            let _ = tx.send(result);
        });
        self.model_selection = Some(rx);
        Ok(())
    }
    pub fn poll_model_selection(&mut self) -> Option<Result<String, String>> {
        let rx = self.model_selection.as_ref()?;
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std_mpsc::TryRecvError::Empty) => return None,
            Err(std_mpsc::TryRecvError::Disconnected) => Err("model selection stopped".into()),
        };
        self.model_selection = None;
        Some(result)
    }
    pub fn set_effort(&self, effort: Option<String>) -> Result<(), String> {
        self.session.set_effort(effort).map_err(err)
    }
    pub fn mode(&self) -> Mode {
        Mode::from_engine(
            self.pending_mode
                .or(self.settings().mode)
                .unwrap_or(self.store.snapshot().config.permissions.mode),
        )
    }
    /// An explicit user choice: persisted (and materializes the session).
    pub fn set_mode(&mut self, mode: Mode) -> Result<(), String> {
        if let Some(e) = self.exclusive()
            && e.safety.is_none()
            && matches!(mode, Mode::Auto | Mode::Unattended)
        {
            return Err(format!(
                "{} needs a safety model, and /exclusive runs without one: manual and yolo only (/exclusive to change)",
                mode.as_str()
            ));
        }
        self.pending_mode = None;
        self.session.set_mode(mode.engine()).map_err(err)
    }
    fn apply_pending(&mut self) {
        if let Some(mode) = self.pending_mode.take() {
            let _ = self.session.set_mode(mode);
        }
    }
    pub fn subagent_disabled(&self, model_id: &str) -> bool {
        self.settings().disabled_models.contains(model_id)
    }
    pub fn provider_subagent_disabled(&self, provider: &str) -> bool {
        self.settings().disabled_providers.contains(provider)
    }
    fn toggle(set: &mut BTreeSet<String>, key: &str) {
        if !set.remove(key) {
            set.insert(key.to_string());
        }
    }
    pub fn toggle_subagent_model(&self, model_id: &str) -> Result<(), String> {
        let s = self.settings();
        let mut models = s.disabled_models;
        Self::toggle(&mut models, model_id);
        self.session
            .set_subagent_toggles(models, s.disabled_providers)
            .map_err(err)
    }
    pub fn toggle_subagent_provider(&self, provider: &str) -> Result<(), String> {
        let s = self.settings();
        let mut providers = s.disabled_providers;
        Self::toggle(&mut providers, provider);
        self.session
            .set_subagent_toggles(s.disabled_models, providers)
            .map_err(err)
    }

    /// Session total cost (whole tree, restored totals included).
    pub fn cost(&self) -> String {
        cost_of(&self.session.totals())
    }
    // ---- skills as slash commands ----------------------------------

    pub fn skills(&self) -> Vec<SkillView> {
        self.session
            .skills()
            .unwrap_or_default()
            .into_iter()
            .map(|s| SkillView {
                name: s.name,
                description: s.description,
                user_only: s.disable_model_invocation,
                shadows_global: s.shadows_global,
            })
            .collect()
    }
    /// Send a skill (its SKILL.md body and the args) as a user message.
    pub fn invoke_skill(&mut self, name: &str, args: &str) -> Result<(), String> {
        self.apply_pending();
        drain(self.session.invoke_skill(name, args).map_err(err)?);
        Ok(())
    }
    /// The tagged message a skill sends, for queueing while a turn runs.
    pub fn skill_message(&self, name: &str, args: &str) -> Result<String, String> {
        self.session.skill_message(name, args).map_err(err)
    }

    // ---- idle compaction, reviewer guidance, discovery ---------

    /// The idle hook, called about once a second. `busy` (typing, an
    /// open dialog) restarts the engine's idle timer. True when the engine
    /// started compacting root.
    pub fn idle_tick(&mut self, busy: bool) -> bool {
        match self.session.idle_tick(busy) {
            Some(rx) => {
                drain(rx);
                true
            }
            None => false,
        }
    }
    pub fn pending_guidance(&self) -> Vec<GuidanceView> {
        self.session
            .pending_reviewer_guidance()
            .into_iter()
            .map(guidance_view)
            .collect()
    }
    // ---- scopes, subscriptions, guidance; prune; local commands ----------

    pub fn pending_scopes(&self) -> Vec<ScopeView> {
        self.session
            .pending_scope_requests()
            .into_iter()
            .map(scope_view)
            .collect()
    }
    /// Answer a scope request as its owner. Applying runs the write through
    /// the permission gate, so it completes in the background; its state
    /// arrives as [`UiEvent::Scope`]. An error comes back on `done`.
    pub fn resolve_scope(&self, id: &str, choice: ScopeChoice, done: std_mpsc::Sender<String>) {
        use code_engine::scopes::ScopeDecision;
        let decision = match choice {
            ScopeChoice::Apply => ScopeDecision::Apply,
            ScopeChoice::Reject(reason) => ScopeDecision::Reject { reason },
            ScopeChoice::Grant(scope) => ScopeDecision::Grant { scope },
        };
        let session = self.session.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            if let Err(e) = session.resolve_scope_request(&id, decision).await {
                let _ = done.send(format!(
                    "scope request {}: {}",
                    id.chars().take(8).collect::<String>(),
                    e.message
                ));
            }
        });
    }
    /// Each agent's write scope (paths, exclusive), root included.
    pub fn scopes(&self) -> std::collections::BTreeMap<String, (Vec<PathBuf>, bool)> {
        self.session
            .agent_scopes()
            .into_iter()
            .map(|(k, v)| (k, (v.paths, v.exclusive)))
            .collect()
    }
    pub fn subscriptions(&self) -> std::collections::BTreeMap<String, Vec<PathBuf>> {
        self.session.subscriptions()
    }
    /// Selections, chronological, optionally for one agent.
    pub fn guidance_selections(&self, agent: Option<&str>) -> Vec<GuidanceSel> {
        self.session
            .guidance_selections(agent)
            .into_iter()
            .map(guidance_sel)
            .collect()
    }
    /// `guidance.enabled` and the selector model override, from config.
    pub fn guidance_config(&self) -> (bool, Option<String>, usize) {
        // Read fresh: the engine re-reads config.json every turn.
        let fresh = ConfigStore::load(self.directories.clone()).ok();
        let g = &fresh
            .as_ref()
            .unwrap_or(&self.store)
            .snapshot()
            .config
            .guidance;
        (g.enabled, g.model.clone(), g.max_items)
    }
    pub fn prune_preview(&self) -> Result<PruneView, String> {
        self.session.prune_preview().map(prune_view).map_err(err)
    }
    pub fn prune(&mut self) {
        drain(self.session.prune());
    }
    /// Tell `agent` that the user ran a local command (no output).
    pub fn post_local_run(&self, agent: &str, run: &LocalRun) -> Result<(), String> {
        self.session
            .post_local_command(
                agent,
                &run.command,
                run.exit_code,
                run.duration_ms,
                run.output_bytes,
            )
            .map_err(err)
    }

    pub fn resolve_guidance(&self, id: &str, trust: bool) -> Result<(), String> {
        self.session
            .resolve_reviewer_guidance(id, trust)
            .map_err(err)
    }
    /// Background discovery finished: re-read the configuration (it only
    /// reads static models and fresh discovery caches, never runs commands).
    pub fn discovery_updated(&mut self) {
        if let Ok(store) = ConfigStore::load(self.directories.clone()) {
            self.store = store;
        }
    }

    // ---- criteria and /goal -------------------------------

    /// An agent's criteria contract (empty when it set none).
    pub fn contract(&self, agent: &str) -> ContractView {
        goal::contract(&self.session.contract(agent))
    }
    /// The session's `/goal` run, if one was ever started.
    pub fn goal(&self) -> Option<GoalView> {
        self.session.goal_status().map(|g| goal::goal(&g))
    }
    /// Dialogs waiting from before an open or resume: a draft, user-owned
    /// proposals, a root roadblock.
    pub fn goal_requests(&self) -> Vec<UiEvent> {
        self.session
            .goal_requests()
            .into_iter()
            .filter_map(translate)
            .collect()
    }
    pub fn goal_start(&mut self, text: &str) -> Result<(), String> {
        self.apply_pending();
        drain(self.session.goal_start(text).map_err(err)?);
        Ok(())
    }
    /// Approve the pending draft, edited or as drafted.
    pub fn goal_approve(&mut self, draft: Option<&ContractDraft>) -> Result<(), String> {
        let base = self.session.contract("root");
        let contract = match draft {
            Some(d) => d.apply(&base),
            None => base,
        };
        drain(self.session.goal_approve(contract).map_err(err)?);
        Ok(())
    }
    pub fn goal_cancel_draft(&self) -> Result<(), String> {
        self.session.goal_cancel_draft().map_err(err)
    }
    pub fn goal_pause(&self) -> Result<(), String> {
        self.session.goal_pause().map_err(err)
    }
    pub fn goal_resume(&mut self) -> Result<(), String> {
        drain(self.session.goal_resume().map_err(err)?);
        Ok(())
    }
    pub fn goal_edit(&self, draft: &ContractDraft) -> Result<(), String> {
        let base = self.session.contract("root");
        self.session.goal_edit(draft.apply(&base)).map_err(err)
    }
    pub fn goal_clear(&self) -> Result<(), String> {
        self.session.goal_clear().map_err(err)
    }
    /// accept, edit (with the new text) or reject a user-owned proposal.
    pub fn resolve_proposal(
        &self,
        agent: &str,
        proposal: &str,
        resolution: &str,
        text: Option<String>,
    ) -> Result<(), String> {
        self.session
            .resolve_proposal(agent, proposal, resolution, text)
            .map_err(err)
    }
    /// pivot (a message and/or contract edits), stop, or override (a note).
    /// True when the resolution started a turn.
    pub fn resolve_roadblock(
        &mut self,
        roadblock: &str,
        resolution: &str,
        text: Option<String>,
        draft: Option<&ContractDraft>,
    ) -> Result<bool, String> {
        let base = self.session.contract("root");
        let contract = draft.map(|d| d.apply(&base));
        match self
            .session
            .resolve_roadblock(roadblock, resolution, text, contract)
            .map_err(err)?
        {
            Some(rx) => {
                drain(rx);
                Ok(true)
            }
            None => Ok(false),
        }
    }
    /// The evaluator and skeptic's calls (the "goal review" `/cost` row).
    pub fn goal_review_totals(&self) -> Totals {
        totals_view(&self.session.goal_review_totals())
    }
    pub fn is_active(&self) -> bool {
        self.session.is_active()
    }
    /// Unanswered questions addressed to the user.
    pub fn pending_questions(&self) -> Vec<QuestionView> {
        self.session
            .pending_questions()
            .into_iter()
            .filter(|q| q.recipient.is_none())
            .map(|q| QuestionView {
                questions: structured_items(&q),
                id: q.id,
                asker: q.asker,
                breadcrumb: q.breadcrumb,
                note: q.note,
                blocking: q.blocking,
            })
            .collect()
    }
    /// Submit the engine's real structured answer set, preserving stable item IDs.
    pub fn answer_questions(
        &self,
        id: &str,
        items: &[QItem],
        answers: &[QAnswer],
    ) -> Result<bool, String> {
        if items.len() != answers.len() {
            return Err("submit an answer or skip for every question".into());
        }
        let answers = items
            .iter()
            .zip(answers)
            .map(|(item, answer)| {
                let mut result = code_engine::schema::QuestionAnswer {
                    id: item.id.clone().ok_or("question item has no engine ID")?,
                    ..Default::default()
                };
                match answer {
                    QAnswer::Skipped => result.skipped = true,
                    QAnswer::Text(text) => result.text = Some(text.clone()),
                    QAnswer::Choice { labels, other } => {
                        result.labels = labels.clone();
                        result.other = other.clone();
                    }
                    QAnswer::Boolean { value, note } => {
                        result.boolean = Some(*value);
                        result.note = note.clone();
                    }
                }
                Ok(result)
            })
            .collect::<Result<Vec<_>, String>>()?;
        let active = self.session.is_active();
        self.session.answer_questions(id, answers).map_err(err)?;
        Ok(!active && self.session.is_active())
    }
    pub fn discuss_question(&self, id: &str) -> Result<(), String> {
        self.session.discuss_question(id).map_err(err)
    }
    pub fn open_fork_chat(&self, id: &str) -> Result<String, String> {
        self.session.open_question_fork(id).map_err(err)
    }
    /// `/btw`: open or reopen `agent`'s side chat (a hidden fork).
    pub fn open_side_chat(&self, agent: &str) -> Result<String, String> {
        self.session.open_side_chat(agent).map_err(err)
    }
    pub fn fork_chat_send(&self, fork: &str, text: String) -> Result<(), String> {
        self.session.fork_chat_send(fork, text).map_err(err)
    }
    pub fn exclusive(&self) -> Option<Exclusive> {
        self.session.exclusive().map(|ex| Exclusive {
            safety: ex.safety_model().map(str::to_owned),
            model: ex.model,
            block_remote: ex.block_network,
        })
    }
    pub fn set_exclusive(&mut self, ex: Option<Exclusive>) -> Result<(), String> {
        let no_safety = ex.as_ref().is_some_and(|ex| ex.safety.is_none());
        self.session
            .set_exclusive(ex.map(|ex| code_engine::Exclusive {
                safety: ex.safety.map(|id| {
                    if id == ex.model {
                        code_engine::ExclusiveSafety::SameModel
                    } else {
                        code_engine::ExclusiveSafety::Model(id)
                    }
                }),
                model: ex.model,
                block_network: ex.block_remote,
            }))
            .map_err(err)?;
        if no_safety
            && matches!(
                self.pending_mode,
                Some(PermissionMode::Auto | PermissionMode::Unattended)
            )
        {
            self.set_mode(Mode::Manual)?;
        }
        Ok(())
    }
    pub fn respond(&self, id: &str, choice: ApprovalChoice) -> Result<(), String> {
        let decision = match choice {
            ApprovalChoice::Once => ApprovalDecision::Once,
            ApprovalChoice::Session => ApprovalDecision::Session,
            ApprovalChoice::Deny(feedback) => ApprovalDecision::Deny { feedback },
        };
        self.session.respond_approval(id, decision).map_err(err)
    }
    pub fn cancel(&self) {
        self.session.cancel();
    }
    /// Start a turn. Events arrive on the session subscription; the per-action
    /// receiver is drained by a task (dropping it would cancel the turn).
    /// Attachment chips travel as `<attachment>` tags; the engine
    /// imports complete tags into the session `assets/` and strips them from
    /// the text. Text files are inlined for every model; binaries
    /// need the model's modality or `file` input.
    pub fn send(&mut self, prompt: String) {
        self.apply_pending();
        drain(self.session.send(prompt));
    }
    pub fn compact(&mut self, focus: Option<String>) {
        self.apply_pending();
        drain(self.session.compact(focus));
    }

    // ---- agent tree and jobs --------------------------------------------

    /// Subagents of the session, oldest first.
    pub fn agents(&self) -> Vec<AgentView> {
        let executions: std::collections::HashMap<_, _> = self
            .session
            .agent_executions()
            .into_iter()
            .map(|e| (e.agent_id.clone(), e))
            .collect();
        let mut out: Vec<AgentView> = self
            .session
            .agents()
            .into_iter()
            .filter_map(|a| {
                let parent = a.parent_id?;
                let execution = executions.get(&a.agent_id);
                Some(AgentView {
                    task: execution.map(|e| e.task.clone()).unwrap_or_default(),
                    background: execution.is_some_and(|e| e.background),
                    id: a.agent_id,
                    parent,
                    role: a.agent,
                    model: a.model,
                    status: a.status,
                    created: a.created_at,
                    call_id: None,
                })
            })
            .collect();
        out.sort_by(|a, b| a.created.cmp(&b.created));
        out
    }
    /// Background jobs of the session, oldest first.
    pub fn jobs(&self) -> Vec<JobView> {
        let mut out: Vec<JobView> = self.session.jobs().into_iter().map(job_view).collect();
        out.sort_by(|a, b| a.started.cmp(&b.started));
        out
    }
    /// Configured MCP servers with their session state.
    pub fn mcp_status(&self) -> Vec<McpView> {
        self.session
            .mcp_status()
            .into_iter()
            .map(|s| McpView {
                name: s.name,
                description: s.description,
                enabled: s.enabled,
                state: s.connection_state,
                tools: s.tool_count,
                error: s.last_error,
            })
            .collect()
    }
    /// Startup health check of the enabled MCP servers: (server,
    /// problem) for each one that is not usable.
    ///
    /// subscribe() starts Session::mcp_health_check once on each open.
    /// This accessor exposes its results to the existing startup notice flow.
    pub fn mcp_health(&self) -> Vec<(String, String)> {
        self.mcp_status()
            .into_iter()
            .filter(|m| m.enabled && (m.state == "error" || m.error.is_some()))
            .map(|m| {
                let why = m
                    .error
                    .as_deref()
                    .and_then(|e| e.lines().next())
                    .unwrap_or("connection failed")
                    .to_string();
                (m.name, why)
            })
            .collect()
    }
    pub fn set_mcp_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        self.session.set_mcp_enabled(name, enabled).map_err(err)
    }
    /// The session title: a user title, else the automatic one.
    pub fn title(&self) -> Option<String> {
        Some(self.session.title()).filter(|t| !t.trim().is_empty())
    }
    /// Set a user title (persisted; stops automatic titling). The
    /// engine emits `TitleChanged`.
    pub fn rename(&mut self, title: &str) -> Result<(), String> {
        let title: String = title.trim().chars().take(60).collect();
        if title.is_empty() {
            return Err("a title can't be empty".into());
        }
        self.session.rename(title).map_err(err)
    }
    /// Numeric totals of one agent's own model calls.
    pub fn agent_totals(&self, id: &str) -> Totals {
        totals_view(&self.session.agent_totals(id))
    }
    /// The session safety model, persisted in `settings.json`; new
    /// sessions default from `permissions.autoModel`.
    pub fn safety_model(&self) -> Option<String> {
        self.session.safety_model()
    }
    /// Validates (exists, text in and out), persists and emits
    /// `setting_change`.
    pub fn set_safety_model(&mut self, id: &str) -> Result<(), String> {
        self.session.set_safety_model(id).map_err(err)
    }
    /// `config.modelSwitchWarnMinutes` (0 is off).
    pub fn switch_warn_minutes(&self) -> u64 {
        self.store.snapshot().config.model_switch_warn_minutes
    }
    /// Minutes since root's last user message, from the persisted records.
    pub fn last_message_minutes(&self) -> Option<f64> {
        let ts = self
            .current_context("root")
            .iter()
            .filter(|e| matches!(e.event, HistoryEvent::User { .. }))
            .map(|e| e.ts.clone())
            .max()?;
        let then = chrono::DateTime::parse_from_rfc3339(&ts).ok()?;
        let secs = (chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_seconds();
        Some(secs.max(0) as f64 / 60.0)
    }
    /// The context meter for a session just opened: the last root
    /// author call's input plus its output (the reply is in context now)
    /// when it used the current model; otherwise a ~4 chars/token estimate
    /// of the current context.
    pub fn resume_context(&self) -> ResumeContext {
        self.resume_agent_context("root")
    }
    /// Restore a viewed child's meter using its own model and current context.
    pub fn resume_agent_context(&self, agent: &str) -> ResumeContext {
        let model = if agent == "root" {
            self.active_model()
        } else {
            self.session
                .agents()
                .into_iter()
                .find(|record| record.agent_id == agent)
                .and_then(|record| record.model)
        };
        let catalog = self.catalog();
        let info = model.as_deref().and_then(|id| catalog.get(id));
        let window = if agent == "root" {
            self.session.context_window()
        } else {
            info.and_then(|model| model.context_window)
        };
        let unknown = info.is_some_and(|model| model.context_unknown);
        let current = self.current_context(agent);
        if current.is_empty() {
            return ResumeContext::default();
        }
        let last = current.iter().rev().find_map(|e| match &e.event {
            HistoryEvent::Usage { usage } if usage.kind == "agent" && !usage.partial => {
                Some(usage.clone())
            }
            _ => None,
        });
        // A provider-reported meter wins; without one, an unknown
        // provider's context stays unknown rather than estimated.
        if let Some(u) = &last
            && Some(&u.model) == model.as_ref()
            && let (Some(used), Some(w)) = (u.context_used, u.context_window)
        {
            return ResumeContext {
                used,
                window: Some(w),
                estimate: false,
                unknown: false,
            };
        }
        if unknown {
            return ResumeContext {
                unknown: true,
                ..ResumeContext::default()
            };
        }
        if let Some(u) = last
            && Some(&u.model) == model.as_ref()
            && let Some(input) = u.tokens.input_tokens
        {
            return ResumeContext {
                used: input + u.tokens.output_tokens.unwrap_or(0),
                window,
                estimate: false,
                unknown: false,
            };
        }
        let chars: usize = current
            .iter()
            .map(|e| match &e.event {
                HistoryEvent::Usage { .. } => 0,
                ev => serde_json::to_string(ev).map(|s| s.len()).unwrap_or(0),
            })
            .sum();
        ResumeContext {
            used: (chars / 4) as u64,
            window,
            estimate: chars > 0,
            unknown: false,
        }
    }
    /// Cost of one agent's own model calls.
    /// Work that quitting or switching sessions would stop: running background
    /// jobs (other than subagent jobs) and live subagents.
    pub fn running_work(&self) -> (usize, usize) {
        let work = self.session.running_work();
        let jobs = work.jobs.iter().filter(|j| j.kind != "task").count();
        (jobs, work.children.len())
    }
    // ---- queued messages ---------------------------------------

    pub fn queue_effect(&self, agent: &str, class: QueueClass) -> &'static str {
        match class {
            QueueClass::NextTurn if agent == "root" => "sent when the turn ends",
            QueueClass::NextTurn => "sent with its next task",
            QueueClass::Steer => "sent at the next boundary",
            QueueClass::Interrupt => "stops the agent and sends now",
        }
    }
    pub fn queue_message(
        &mut self,
        agent: &str,
        text: String,
        class: QueueClass,
    ) -> Result<QueueOutcome, String> {
        let id = self
            .session
            .queue_message(agent, text, class.config())
            .map_err(err)?;
        Ok(QueueOutcome::Held(id))
    }
    /// Editing removes the old entry; submitting creates a new ID at the tail.
    pub fn requeue(&mut self, item: Queued, _at: usize) -> Result<(), String> {
        self.queue_message(&item.agent, item.text, item.class)
            .map(|_| ())
    }
    pub fn reclassify(&mut self, id: u64, class: QueueClass) -> Result<(), String> {
        self.session.reclassify(id, class.config()).map_err(err)
    }
    pub fn unqueue(&mut self, id: u64) -> Option<(Queued, usize)> {
        let (item, at) = self.session.unqueue(id).ok()??;
        Some((queued_view(item, &self.assets_dir()), at))
    }
    pub fn queued(&self, agent: &str) -> Vec<Queued> {
        self.session
            .queued_messages(agent)
            .into_iter()
            .map(|item| queued_view(item, &self.assets_dir()))
            .collect()
    }
    pub fn queued_count(&self) -> usize {
        self.session
            .agents()
            .iter()
            .map(|agent| self.session.queued_messages(&agent.agent_id).len())
            .sum()
    }
    pub fn drain_queue(&mut self, agent: &str) -> Vec<Queued> {
        self.queued(agent)
            .into_iter()
            .filter_map(|item| self.unqueue(item.id).map(|(item, _)| item))
            .collect()
    }

    /// Kill a background job (and a background subagent's tree) in the background.
    pub fn kill_job(&self, id: String) {
        let session = self.session.clone();
        tokio::spawn(async move {
            let _ = session.kill_job(&id).await;
        });
    }
    /// SESSION/assets/: imported attachments.
    fn assets_dir(&self) -> PathBuf {
        self.directories
            .data
            .join("sessions")
            .join(self.session.id())
            .join("assets")
    }
    fn job_log(&self, id: &str) -> PathBuf {
        self.directories
            .data
            .join("sessions")
            .join(self.session.id())
            .join("jobs")
            .join(format!("{id}.log"))
    }
    /// A job's output so far (the last 4 MB at most).
    pub fn job_output(&self, id: &str) -> String {
        use std::io::{Read, Seek, SeekFrom};
        const CAP: u64 = 4 << 20;
        let Ok(mut file) = std::fs::File::open(self.job_log(id)) else {
            return String::new();
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.seek(SeekFrom::Start(len.saturating_sub(CAP)));
        let mut bytes = vec![];
        let _ = file.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }
    /// The last non-empty line of a job's output.
    pub fn job_tail(&self, id: &str) -> String {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(self.job_log(id)) else {
            return String::new();
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.seek(SeekFrom::Start(len.saturating_sub(2048)));
        let mut bytes = vec![];
        let _ = file.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

fn drain(mut rx: mpsc::UnboundedReceiver<EngineEvent>) {
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
}

fn open(
    directories: &Directories,
    cwd: &Path,
    target: Target,
    options: SessionOptions,
) -> Result<Session, OpenError> {
    let store = ConfigStore::load(directories.clone()).map_err(|e| e.to_string())?;
    let (id, result) = match target {
        Target::New => (
            String::new(),
            Session::create(store, cwd.to_path_buf(), options),
        ),
        Target::Resume(id) => {
            let r = Session::resume(store, &id, options);
            (id, r)
        }
        Target::Claim(id) => {
            let r = Session::claim(store, &id, options);
            (id, r)
        }
        Target::Fork(id, point) => {
            let point = point.map(|(context, line)| ForkPoint {
                session_id: id.clone(),
                agent_id: "root".into(),
                context,
                line,
            });
            let r = Session::fork(store, &id, point, options);
            (id, r)
        }
    };
    result.map_err(|e| match e.kind {
        ErrorKind::LockConflict => match lock_info(directories, &id) {
            Some(info) => OpenError::Locked(info),
            None => OpenError::Other(e.message),
        },
        _ => OpenError::Other(e.message),
    })
}

fn lock_info(directories: &Directories, id: &str) -> Option<LockInfo> {
    let status = sessions::lock_status(&directories.data, id).ok()?;
    let holder = status.holder?;
    Some(LockInfo {
        session_id: id.to_string(),
        title: status.title,
        pid: holder.pid,
        host: holder.host,
        age: age(&holder.started_at),
        alive: status.pid_alive,
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) struct TestDir(pub PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn engine() -> (TestDir, Engine, EventRx) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "code-tui-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let directories = Directories {
            config: path.join("config"),
            data: path.join("data"),
            cache: path.join("cache"),
        };
        std::fs::create_dir_all(&directories.config).unwrap();
        std::fs::write(
            directories.config.join("config.json"),
            r#"{"skillsDirs":[]}"#,
        )
        .unwrap();
        let store = ConfigStore::load(directories.clone()).unwrap();
        let session =
            Session::create(store.clone(), path.clone(), SessionOptions::default()).unwrap();
        let (_, rx) = mpsc::unbounded_channel();
        let engine = Engine {
            directories,
            store,
            session,
            cwd: path.clone(),
            pending_mode: None,
            retired: vec![],
            deferred_model: None,
            discovery: None,
            model_selection: None,
            terminal_issue: None,
        };
        (TestDir(path), engine, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_attachment_survives_composer_edit_and_replay() {
        let assets = Path::new("/session/assets");
        let item = code_engine::QueuedMessage {
            id: 7,
            agent: "root".into(),
            text: "look here".into(),
            attachments: vec![code_engine::schema::Attachment {
                path: "image.png".into(),
                name: "image.png".into(),
                mime_type: "image/png".into(),
            }],
            class: code_config::QueueClass::NextTurn,
        };
        let queued = queued_view(item.clone(), assets);
        let mut composer = crate::composer::Composer::default();
        composer.set_wire(&queued.text);
        assert!(composer.chip(1).is_none());
        assert_eq!(composer.chip(0).unwrap().full, "/session/assets/image.png");
        assert_eq!(composer.chip(0).unwrap().attach.as_deref(), Some("image"));
        assert_eq!(
            crate::paste::attachments(&composer.wire()),
            crate::paste::attachments(&user_wire(&item.text, &item.attachments, assets))
        );
        composer.insert(" edited");
        let wire = composer.wire();
        composer.set_wire(&wire);
        assert!(composer.chip(1).is_none());
        assert_eq!(composer.chip(0).unwrap().full, "/session/assets/image.png");
        assert!(composer.text().ends_with(" edited"));
    }
}
