//! Engine adapter: the only module that touches `code_engine` / `code_config`
//! types. Everything else in the TUI speaks the plain types defined here, so
//! engine changes (subagents, jobs, MCP) stay local.
use crate::usage::{self, Budget, Limit, ProviderUsage};
use code_config::{Billing, ConfigStore, Directories, Modality, ModelToggles, PermissionMode};
pub use code_engine::ForkChatMessage;
pub use code_engine::schema::{FileEdit, ToolCall, ToolResultData, ToolResultDetails};
use code_engine::{
    Session, SessionOptions,
    schema::{
        ApprovalDecision, EngineError, EngineEvent, ErrorKind, EventEnvelope, HistoryEvent,
        JobRecord,
    },
    sessions::{self, ForkPoint, SessionSettings},
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use tokio::sync::mpsc;

mod goal;
mod terminal;
mod tree;
pub use goal::{
    ContractDraft, ContractView, CriterionView, DraftItem, GoalView, ProposalView, RoadblockView,
    Section,
};
pub use terminal::{TermCmd, TermInfo, TermMsg, TermRx, TermTx};
pub use tree::{AgentDone, DecisionView, Review, TreeRow, agent_filters};

// ---- plain frontend types ------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Manual,
    Auto,
    Unattended,
    Yolo,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Manual, Mode::Auto, Mode::Unattended, Mode::Yolo];
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Manual => "manual",
            Mode::Auto => "auto",
            Mode::Unattended => "unattended",
            Mode::Yolo => "yolo",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
    pub fn describe(self) -> &'static str {
        match self {
            Mode::Manual => "ask before bash, scripts and outside-cwd access",
            Mode::Auto => "safety model approves; unsafe calls ask you",
            Mode::Unattended => "safety model decides; unsafe calls are rejected",
            Mode::Yolo => "run everything without asking",
        }
    }
    fn engine(self) -> PermissionMode {
        match self {
            Mode::Manual => PermissionMode::Manual,
            Mode::Auto => PermissionMode::Auto,
            Mode::Unattended => PermissionMode::Unattended,
            Mode::Yolo => PermissionMode::Yolo,
        }
    }
    fn from_engine(m: PermissionMode) -> Self {
        match m {
            PermissionMode::Manual => Mode::Manual,
            PermissionMode::Auto => Mode::Auto,
            PermissionMode::Unattended => Mode::Unattended,
            PermissionMode::Yolo => Mode::Yolo,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalView {
    pub call_id: String,
    pub parent_call_id: Option<String>,
    pub id: String,
    /// Requesting agent (`root` or a subagent id).
    pub agent: String,
    /// Background job the request runs under, if any.
    pub job: Option<String>,
    pub tool: String,
    pub breadcrumb: String,
    pub target: Option<String>,
    pub diff: Option<String>,
    pub command: Option<String>,
    pub reason: String,
    pub grant_key: Option<String>,
    /// Sidecar verdict (`unsafe`, …) and hint when the safety model escalated.
    pub verdict: Option<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct QuestionView {
    pub id: String,
    /// Agent that asked.
    pub asker: String,
    pub breadcrumb: String,
    /// Escalation note from the agent that passed the question up.
    pub note: Option<String>,
    /// The asker is parked until answered (D51).
    pub blocking: bool,
    /// The structured questions of the call (D62).
    pub questions: Vec<QItem>,
}

/// D62 question kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QKind {
    Text,
    Choice,
    Boolean,
}

#[derive(Debug, Clone)]
pub struct QOption {
    pub label: String,
    pub description: Option<String>,
}

/// One question of a `question` call (D62).
#[derive(Debug, Clone)]
pub struct QItem {
    pub id: Option<String>,
    pub text: String,
    pub kind: QKind,
    pub options: Vec<QOption>,
    pub multi: bool,
}

/// The answer to one question (D62): chosen labels plus other text, a
/// boolean plus a note, text, or skipped.
#[derive(Debug, Clone)]
pub enum QAnswer {
    Skipped,
    Text(String),
    Choice {
        labels: Vec<String>,
        other: Option<String>,
    },
    Boolean {
        value: bool,
        note: Option<String>,
    },
}

impl QAnswer {
    /// One line: `A, B; other: x` · `yes (note)` · text · `skipped`.
    pub fn summary(&self) -> String {
        match self {
            QAnswer::Skipped => "skipped".into(),
            QAnswer::Text(t) => t.clone(),
            QAnswer::Choice { labels, other } => {
                let mut parts = labels.clone();
                if let Some(o) = other {
                    parts.push(format!("other: {o}"));
                }
                parts.join(", ")
            }
            QAnswer::Boolean { value, note } => match note {
                Some(n) => format!("{} ({n})", if *value { "yes" } else { "no" }),
                None => (if *value { "yes" } else { "no" }).into(),
            },
        }
    }
}

/// Questions of a call: the engine's structured D62 questions; a record
/// without them (a plain-text question) falls back to its text.
fn structured_items(q: &code_engine::schema::QuestionRecord) -> Vec<QItem> {
    if q.questions.is_empty() {
        return question_items(&q.text);
    }
    q.questions
        .iter()
        .map(|q| QItem {
            id: Some(q.id.clone()),
            text: q.text.clone(),
            multi: q.multi_select,
            kind: match q.kind {
                code_engine::schema::QuestionKind::Text => QKind::Text,
                code_engine::schema::QuestionKind::Choice => QKind::Choice,
                code_engine::schema::QuestionKind::Boolean => QKind::Boolean,
            },
            options: q
                .options
                .iter()
                .map(|o| QOption {
                    label: o.label.clone(),
                    description: o.description.clone(),
                })
                .collect(),
        })
        .collect()
}
fn question_items(text: &str) -> Vec<QItem> {
    let parsed = serde_json::from_str::<Value>(text.trim())
        .ok()
        .and_then(|v| {
            let qs = v.get("questions")?.as_array()?;
            let items: Vec<QItem> = qs
                .iter()
                .filter_map(|q| {
                    let text = q.get("text")?.as_str()?.to_string();
                    let kind = match q.get("kind").and_then(Value::as_str).unwrap_or("text") {
                        "choice" => QKind::Choice,
                        "boolean" => QKind::Boolean,
                        _ => QKind::Text,
                    };
                    let options = q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|os| {
                            os.iter()
                                .filter_map(|o| {
                                    Some(QOption {
                                        label: o.get("label")?.as_str()?.to_string(),
                                        description: o
                                            .get("description")
                                            .and_then(Value::as_str)
                                            .map(str::to_string),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(QItem {
                        id: q.get("id").and_then(Value::as_str).map(str::to_string),
                        text,
                        kind,
                        options,
                        multi: q
                            .get("multiSelect")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect();
            (!items.is_empty()).then_some(items)
        });
    parsed.unwrap_or_else(|| {
        vec![QItem {
            id: None,
            text: text.to_string(),
            kind: QKind::Text,
            options: vec![],
            multi: false,
        }]
    })
}

/// The answers as one text block (the pre-D62 answer path and the
/// transcript row).
pub fn answers_text(items: &[QItem], answers: &[QAnswer]) -> String {
    if items.len() == 1 {
        return answers.first().map(QAnswer::summary).unwrap_or_default();
    }
    items
        .iter()
        .zip(answers)
        .map(|(q, a)| {
            format!(
                "{}: {}",
                q.id.clone().unwrap_or_else(|| q
                    .text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string()),
                a.summary()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A subagent in the session tree.
#[derive(Debug, Clone)]
pub struct AgentView {
    pub id: String,
    pub parent: String,
    pub role: String,
    pub model: Option<String>,
    pub status: String,
    pub task: String,
    pub background: bool,
    pub created: String,
    /// The `task` call that started it. The engine does not report it
    /// (`AgentExecution` has no call id), so the TUI links in call order and
    /// corrects from the result's `agentId`; kept for when it does.
    pub call_id: Option<String>,
}

/// A background job (bash command or background subagent).
#[derive(Debug, Clone)]
pub struct JobView {
    pub id: String,
    pub owner: String,
    pub kind: String,
    pub status: String,
    pub command: String,
    /// The child agent of a background `task` job.
    pub agent: Option<String>,
    pub started: String,
    pub updated: String,
    pub report: Option<String>,
}

impl JobView {
    pub fn running(&self) -> bool {
        self.status == "running"
    }
}

fn job_view(j: JobRecord) -> JobView {
    JobView {
        id: j.id,
        owner: j.owner,
        kind: j.kind,
        status: j.status,
        command: j.command,
        agent: j.agent_id,
        started: j.started,
        updated: j.updated,
        report: j.report,
    }
}

/// Whether an agent status means the agent loop is still alive.
pub fn alive(status: &str) -> bool {
    matches!(status, "running" | "waiting" | "compacting") || status.starts_with("queued")
}

/// An agent status for display. D66 adds `queued` (waiting for a slot on a
/// model or provider with `maxConcurrentContexts`); a queue position shows
/// as `queued (2nd)`.
///
/// The engine emits `queued:N`; this also accepts older
/// `queued`, `queued:N` and `queued N`.
pub fn status_text(status: &str) -> String {
    let Some(rest) = status.strip_prefix("queued") else {
        return status.to_string();
    };
    let n: Option<usize> = rest.trim_start_matches([':', ' ']).parse().ok();
    match n {
        Some(n) => {
            let suffix = match (n % 10, n % 100) {
                (1, x) if x != 11 => "st",
                (2, x) if x != 12 => "nd",
                (3, x) if x != 13 => "rd",
                _ => "th",
            };
            format!("queued ({n}{suffix})")
        }
        None => "queued".into(),
    }
}

pub enum ApprovalChoice {
    Once,
    Session,
    Deny(String),
}

/// A skill the user can invoke as `/<name>` (D109).
#[derive(Debug, Clone)]
pub struct SkillView {
    pub name: String,
    pub description: String,
    /// `disable-model-invocation: true`: hidden from the model ("you only").
    pub user_only: bool,
    /// A project skill that overrides a global one of the same name.
    pub shadows_global: bool,
}

/// Project reviewer guidance awaiting trust (D107): the exact content.
#[derive(Debug, Clone)]
pub struct GuidanceView {
    pub id: String,
    pub path: PathBuf,
    pub sha256: String,
    pub content: String,
    /// D115: an unapproved change was restored; `content` is the quarantined proposal.
    pub tampered: bool,
    /// D115: the proposal is to delete the file.
    pub deleted: bool,
    /// D121: a project guidance library file, not reviewer guidance.
    pub library: bool,
}

/// A D120 out-of-scope write/edit routed to the scope's owner.
#[derive(Debug, Clone)]
pub struct ScopeView {
    pub id: String,
    pub requester: String,
    pub owner: String,
    pub path: PathBuf,
    /// `write` or `edit`, and its arguments (for the diff).
    pub tool: String,
    pub args: Value,
    /// pending, applying, applied, rejected, granted, stale, failed, unavailable.
    pub status: String,
    /// The requester parked waiting for the answer.
    pub wait: bool,
    pub escalation: Option<String>,
    pub reason: Option<String>,
    pub output: Option<String>,
}

fn scope_view(r: code_engine::scopes::ScopeRequest) -> ScopeView {
    ScopeView {
        id: r.id,
        requester: r.requester,
        owner: r.owner,
        path: r.path,
        tool: r.call.name,
        args: r.call.arguments,
        status: r.status,
        wait: r.wait,
        escalation: r.escalation,
        reason: r.reason,
        output: r.output.map(|o| strip_guidance(strip_ctx(&o))),
    }
}

/// The owner's answer to a scope request (D120).
#[derive(Debug, Clone)]
pub enum ScopeChoice {
    Apply,
    Reject(String),
    Grant(Vec<PathBuf>),
}

/// One changed path of a D120 subscription notice.
#[derive(Debug, Clone)]
pub struct ChangeView {
    pub path: PathBuf,
    /// The agent that wrote it; None is the user's editor or unknown.
    pub source: Option<String>,
    /// The engine's notice text (a bounded diff, or "changed; re-read").
    pub text: String,
}

fn change_views(changes: Vec<code_engine::subscriptions::SubscriptionChange>) -> Vec<ChangeView> {
    changes
        .into_iter()
        .map(|c| ChangeView {
            path: c.path,
            source: c.source,
            text: c.text,
        })
        .collect()
}

/// A D121 guidance selection: what was injected after a write, and why.
#[derive(Debug, Clone)]
pub struct GuidanceSel {
    pub id: String,
    pub agent: String,
    pub context: u32,
    pub model: String,
    /// (id, title, text) of each injected item.
    pub items: Vec<(String, String, String)>,
    pub reason: String,
    pub path: PathBuf,
    pub language: String,
    pub area: String,
    pub first_write: bool,
    pub created: String,
    pub error: Option<String>,
}

fn guidance_sel(s: code_engine::guidance_selector::GuidanceSelection) -> GuidanceSel {
    GuidanceSel {
        id: s.id,
        agent: s.agent_id,
        context: s.context,
        model: s.model,
        items: s
            .items
            .into_iter()
            .map(|i| (i.id, i.title, i.text))
            .collect(),
        reason: s.reason,
        path: s.trigger.path,
        language: s.trigger.language,
        area: s.trigger.area,
        first_write: s.trigger.first_write,
        created: s.created,
        error: s.error,
    }
}

/// A `/prune` preview or result (engine-n).
#[derive(Debug, Clone, Default)]
pub struct PruneView {
    pub before: u64,
    pub after: u64,
    pub saved: u64,
    pub delegations: usize,
    pub tool_outputs: usize,
    pub tail_batches: usize,
    pub cache_break: bool,
}

fn prune_view(r: code_engine::PruneReport) -> PruneView {
    PruneView {
        before: r.tokens_before,
        after: r.tokens_after,
        saved: r.tokens_saved,
        delegations: r.delegations,
        tool_outputs: r.tool_outputs,
        tail_batches: r.tail_batches,
        cache_break: r.cache_break,
    }
}

/// D122: the facts of a `!cmd` run, as the agent was told (never the output).
#[derive(Debug, Clone)]
pub struct LocalRun {
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub output_bytes: u64,
}

fn guidance_view(r: code_engine::ReviewerGuidanceRequest) -> GuidanceView {
    GuidanceView {
        library: r.library,
        id: r.id,
        path: r.path,
        sha256: r.sha256,
        content: r.content,
        tampered: r.tampered,
        deleted: r.deleted,
    }
}

/// Frontend view of one engine event.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// D144/D140 terminal metadata (never its bytes).
    Terminal(TermInfo),
    Text(String),
    Reasoning(String),
    /// A tool call being written: tentative argument fragments.
    ToolDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        args: String,
    },
    ToolStart {
        id: String,
        name: String,
        args: Value,
    },
    /// An inner tool call of a `script` run: started, completed or error.
    ScriptCall {
        job: Option<String>,
        /// The containing script call (engine-m).
        parent: Option<String>,
        call: ToolCall,
        result: Option<ToolResultData>,
        status: String,
    },
    /// A sidecar safety review of a call started (`result` None) or
    /// finished: (safe, verdict, reason) (D46).
    SafetyReview {
        call_id: String,
        model: String,
        result: Option<(bool, String, String)>,
    },
    ToolEnd {
        id: String,
        output: String,
        is_error: bool,
        details: ToolResultDetails,
    },
    /// A steer or interrupt moved foreground call `call_id` to the
    /// background as job `job` (D72).
    MovedToBackground {
        call_id: String,
        job: String,
        /// `steer` or `interrupt`.
        reason: String,
    },
    /// An interrupt (D74) stopped the stream: the partial text is kept,
    /// tentative tool calls are dropped.
    Interrupted,
    /// The engine's queue changed (delivered, reclassified); redraw.
    QueueChanged,
    /// A durably recorded root user message for composer recall.
    UserRecorded(String),
    Todo(Vec<TodoItem>),
    Usage(Usage),
    Retry {
        attempt: usize,
        delay_ms: u64,
        reason: Option<String>,
    },
    StreamReset,
    /// The network dropped: the engine probes and retries by itself (engine-n).
    Reconnecting {
        attempt: usize,
        delay_ms: u64,
    },
    /// D119: a backup model took over for the rest of this turn.
    ModelSwitched {
        from: String,
        to: String,
        reason: String,
    },
    Error {
        kind: &'static str,
        message: String,
    },
    Context {
        used: u64,
        window: Option<u64>,
        reserved: u64,
        warning: bool,
        /// False when the meter's source is `unknown` (a command provider
        /// that reports no context): nothing about the window is known.
        known: bool,
        /// Tokenizer-only or provider-anchored tokenizer estimate.
        estimate: bool,
    },
    Approval(ApprovalView),
    Question(QuestionView),
    Answered(String),
    /// Project reviewer guidance needs the user's trust (D107).
    GuidanceTrust(GuidanceView),
    /// A D120 scope request changed state (upsert by id).
    Scope(ScopeView),
    /// D120: subscribed paths changed (delivered to `agent`).
    Subscription {
        agent: String,
        changes: Vec<ChangeView>,
        overflow: bool,
    },
    /// D121: guidance was selected (or nothing / a failure) after a write.
    Guidance(GuidanceSel),
    /// `/prune` finished.
    Pruned(PruneView),
    /// Root was compacted while the tree was idle (D100).
    IdleCompacted,
    RootCompactionSuggested {
        used: u64,
        window: u64,
    },
    /// A durable tree row (D131/D132) committed in `agent`'s journal.
    Tree {
        agent: String,
        row: TreeRow,
    },
    /// The D128 ledger changed: the open count after the change.
    Decision {
        open_count: usize,
    },
    /// Background model discovery finished (engine-l): re-read the catalog.
    DiscoveryUpdated,
    /// An agent's criteria contract changed (D88).
    ContractChanged(String),
    /// `/goal` drafted a contract: open the approval dialog (D93).
    GoalDraft(ContractView),
    /// The `/goal` run changed (disposition, phase, counters); read it
    /// back with `Engine::goal` (events can trail the state).
    GoalUpdate,
    /// An agent proposes a change to a user-owned criterion (D89).
    ProposalRequest {
        agent: String,
        proposal: ProposalView,
    },
    /// A root roadblock awaits pivot, stop or override (D91), in every mode.
    RoadblockRequest {
        agent: String,
        roadblock: RoadblockView,
    },
    Compacted(String),
    Claimed(String),
    Partial {
        status: String,
        text: String,
    },
    SettingsChanged,
    /// The session title changed (D55), automatically or by `/rename`.
    TitleChanged {
        title: String,
        auto: bool,
    },
    /// A hidden fork (auto title, D56) finished: its purpose and success.
    HiddenFork {
        purpose: String,
        ok: bool,
    },
    TurnEnd {
        stop_reason: String,
    },
    AgentStarted(AgentView),
    AgentStatus {
        id: String,
        status: String,
    },
    AgentFinished {
        id: String,
        status: String,
    },
    /// A subagent's display event.
    Child {
        id: String,
        event: Box<UiEvent>,
    },
    /// A tree message (`user`, `jobs` or an agent id as sender).
    Message {
        from: String,
        to: String,
        text: String,
    },
    Job(JobView),
    /// A question fork's chat event (D64), routed by fork id into the
    /// fork-chat overlay: `opened` (with the saved `messages` as (role,
    /// text)), `user`, `assistant`, `tool` ("name: output"), `status`,
    /// `idle`, `closed` or `error`.
    ForkChat {
        fork: String,
        kind: String,
        text: String,
        messages: Vec<ForkChatMessage>,
        /// `tool_start` / `tool`: the structured call and result (engine-m).
        call: Option<ToolCall>,
        result: Option<ToolResultData>,
    },
    /// A question fork answered question `id` of `asker` (D64).
    ForkAnswered {
        id: String,
        asker: String,
        summary: String,
    },
}

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

pub fn todo_items(value: &Value) -> Vec<TodoItem> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| TodoItem {
                    content: i["content"].as_str().unwrap_or_default().to_string(),
                    status: i["status"].as_str().unwrap_or("pending").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub model: String,
    pub provider: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: Option<f64>,
    /// A live quota report of the call (m7 `UsageRecord.quota`).
    pub quota: Option<Limit>,
}

fn usage_view(u: &code_engine::schema::UsageRecord) -> Usage {
    Usage {
        model: u.model.clone(),
        provider: if u.provider.is_empty() {
            u.model.split('/').next().unwrap_or_default().to_string()
        } else {
            u.provider.clone()
        },
        input: u.tokens.input_tokens.unwrap_or(0),
        output: u.tokens.output_tokens.unwrap_or(0),
        cache_read: u.tokens.cache_read_tokens.unwrap_or(0),
        cache_write: u.tokens.cache_write_tokens.unwrap_or(0),
        cost: u.cost_usd,
        quota: u.quota.as_ref().and_then(usage::quota_limit),
    }
}

#[derive(Debug, Clone)]
pub struct Benchmark {
    pub key: String,
    pub label: String,
    pub higher_is_better: bool,
}

#[derive(Debug, Clone)]
pub struct CostRates {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// `provider/model`
    pub id: String,
    pub provider: String,
    pub provider_label: String,
    pub label: String,
    pub description: String,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub input: Vec<String>,
    pub output: Vec<String>,
    pub tools: bool,
    pub benchmarks: Vec<(String, f64)>,
    pub cost: Option<CostRates>,
    pub provider_enabled: bool,
    /// Config allows subagent use (provider and model flags, text-only output).
    pub subagent_capable: bool,
    /// Text output only, or text plus media: usable as the chat model.
    pub chat_capable: bool,
    /// Billed by subscription (`billing: subscription`).
    pub subscription: bool,
    /// A command model with `history: latest`: context is known only when
    /// the provider reports it, never from the configured window.
    pub context_unknown: bool,
}

/// One configured provider, for `/model` headers and `/usage`.
#[derive(Debug, Clone)]
pub struct ProviderView {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    /// A command provider with model discovery.
    pub discovers: bool,
    /// The last discovery failure (static models are in use).
    pub warning: Option<String>,
    /// Its kind, and whether it came from the 5-minute failure cache with
    /// the time a retry happens by itself: "timeout · cached · retry 14:32".
    pub warning_detail: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub models: Vec<ModelInfo>,
    pub providers: Vec<ProviderView>,
    pub benchmarks: Vec<Benchmark>,
    pub theme: String,
    pub mouse: bool,
    /// `attention`: `bell`, `desktop` or `off`.
    pub attention: String,
    pub vim: bool,
    pub paste_collapse_lines: usize,
    /// `queueDefault` (D73).
    pub queue_default: QueueClass,
    /// Resolved config directory and the variable that chose it, if any.
    pub config_dir: PathBuf,
    pub config_var: Option<&'static str>,
    /// Provider/MCP files skipped because they failed to parse or resolve.
    /// Discovery failures are on `providers` instead.
    pub errors: Vec<String>,
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.id == id)
    }
    pub fn provider(&self, id: &str) -> Option<&ProviderView> {
        self.providers.iter().find(|p| p.id == id)
    }
    /// Explains an empty model list: where providers were looked for, and why.
    pub fn no_models(&self) -> String {
        let dir = self.config_dir.join("providers");
        let via = self
            .config_var
            .map(|v| format!(" ({v} is set)"))
            .unwrap_or_default();
        format!(
            "No models configured. Looked in {}/{via}. Add a provider file there, or unset the variable to use the default location.",
            dir.display()
        )
    }
    pub fn higher_is_better(&self, key: &str) -> bool {
        self.benchmarks
            .iter()
            .find(|b| b.key == key)
            .is_none_or(|b| b.higher_is_better)
    }
}

fn modality(m: &Modality) -> String {
    match m {
        Modality::Text => "text",
        Modality::Image => "image",
        Modality::Video => "video",
        Modality::Audio => "audio",
        Modality::Pdf => "pdf",
    }
    .into()
}

/// Startup flags, the same as headless.
pub struct Launch {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: PathBuf,
    pub resume: Option<String>,
    pub continue_session: bool,
    pub fork: Option<String>,
    pub claim_lock: bool,
    pub mode: Option<String>,
    pub flat: bool,
}

/// Holder of a session lock, for the lock dialog.
#[derive(Debug, Clone)]
pub struct LockInfo {
    pub session_id: String,
    pub title: String,
    pub pid: u32,
    pub host: String,
    pub age: String,
    pub alive: Option<bool>,
}

pub enum OpenError {
    Locked(LockInfo),
    Other(String),
}

impl From<String> for OpenError {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

/// How a queued user message is delivered (D73, D74).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueClass {
    /// Held until the agent's turn ends (root) or its next task (child).
    #[default]
    NextTurn,
    /// Delivered at the next model boundary.
    Steer,
    /// Stops the in-flight generation and is delivered at once.
    Interrupt,
}

impl QueueClass {
    pub fn label(self) -> &'static str {
        match self {
            QueueClass::NextTurn => "next turn",
            QueueClass::Steer => "steer",
            QueueClass::Interrupt => "interrupt",
        }
    }
    /// Enter on an empty composer: next turn → steer → interrupt.
    pub fn escalated(self) -> Option<Self> {
        match self {
            QueueClass::NextTurn => Some(QueueClass::Steer),
            QueueClass::Steer => Some(QueueClass::Interrupt),
            QueueClass::Interrupt => None,
        }
    }
    /// The class toggle: next turn ↔ steer (an interrupt goes back to next turn).
    pub fn toggled(self) -> Self {
        match self {
            QueueClass::NextTurn => QueueClass::Steer,
            QueueClass::Steer | QueueClass::Interrupt => QueueClass::NextTurn,
        }
    }
    fn config(self) -> code_config::QueueClass {
        match self {
            Self::NextTurn => code_config::QueueClass::NextTurn,
            Self::Steer => code_config::QueueClass::Steer,
            Self::Interrupt => code_config::QueueClass::Interrupt,
        }
    }
    fn from_config(c: code_config::QueueClass) -> Self {
        match c {
            code_config::QueueClass::NextTurn => QueueClass::NextTurn,
            code_config::QueueClass::Steer => QueueClass::Steer,
            code_config::QueueClass::Interrupt => QueueClass::Interrupt,
        }
    }
}

/// A queued user message (D73).
#[derive(Clone, Debug)]
pub struct Queued {
    pub id: u64,
    /// `root` or a subagent id.
    pub agent: String,
    /// Composer wire text (paste chips encoded).
    pub text: String,
    pub class: QueueClass,
}

/// What `queue_message` did with a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueOutcome {
    /// Held in the queue under this id.
    Held(u64),
    /// Handed to the agent already (it arrives at its next boundary).
    #[allow(dead_code)]
    Delivered,
}

pub enum Target {
    New,
    Resume(String),
    Claim(String),
    /// Fork a session at its latest point, or before `(context, line)`.
    Fork(String, Option<(u32, usize)>),
}

/// One row of the `/resume` picker.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub age: String,
    pub model: Option<String>,
    pub cost: String,
    pub cwd: String,
    pub locked: bool,
    /// Locked by a process that is gone (a crash): resuming offers a claim.
    pub stale: bool,
}

/// A user message that a fork can rewind to.
#[derive(Debug, Clone)]
pub struct ForkCandidate {
    pub text: String,
    pub context: u32,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub enum ReplayItem {
    /// Text and the journal timestamp (RFC 3339).
    User(String, String),
    Assistant {
        text: String,
        reasoning: String,
        ts: String,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
        details: ToolResultDetails,
    },
    /// A script host call (engine-m): its containing script and result.
    ScriptCall {
        parent: Option<String>,
        call: ToolCall,
        result: Option<ToolResultData>,
    },
    Compacted(String),
    Notice(String),
    /// D119: a backup model took over (from, to, reason).
    Switched {
        from: String,
        to: String,
        reason: String,
    },
    /// A turn error, persisted since engine-l: replayed as prominently as live.
    Error(String),
    /// A foreground call moved to the background (D72): call id, reason.
    Moved {
        id: String,
        reason: String,
    },
    /// A question answered through its fork (D64): question id, summary.
    ForkAnswered {
        id: String,
        summary: String,
    },
    Message {
        from: String,
        to: String,
        text: String,
    },
    Job(JobView),
    Scope(ScopeView),
    Subscription {
        changes: Vec<ChangeView>,
        overflow: bool,
    },
    Guidance(GuidanceSel),
    Pruned(PruneView),
    LocalRun(LocalRun),
    /// A D131/D132 tree row.
    Tree(TreeRow),
    /// D125: the root compaction suggestion (percent, used, window).
    CompactSuggested(u64, u64, u64),
}

/// A resumed transcript: earlier (compacted) context windows and the current one.
#[derive(Default)]
pub struct Replay {
    /// Context files older than `earlier`, not loaded (D103).
    pub older: Vec<u32>,
    pub earlier: Vec<Vec<ReplayItem>>,
    pub current: Vec<ReplayItem>,
    pub usage: Vec<Usage>,
}

pub type EventRx = mpsc::UnboundedReceiver<UiEvent>;

fn err(e: EngineError) -> String {
    e.message
}

pub fn fmt_cost(totals_known: f64, known_calls: u64, unknown_calls: u64) -> String {
    if known_calls == 0 {
        return if unknown_calls > 0 {
            "cost unknown".into()
        } else {
            "$0.00".into()
        };
    }
    let base = if totals_known > 0.0 && totals_known < 0.01 {
        format!("${totals_known:.4}")
    } else {
        format!("${totals_known:.2}")
    };
    if unknown_calls > 0 { base + "+" } else { base }
}

/// One provider's usage from the engine's shared cache (D85, D87):
/// the `cockpit-code.usage/1` document, budget spend over the period from
/// the harness's cost records, and stale/refresh state. A provider with no
/// usage document falls back to its discovery `account` quota.
fn usage_of(
    dirs: &Directories,
    p: &code_config::Provider,
    u: code_engine::usage::ProviderUsage,
) -> ProviderUsage {
    let subscription = !p.models.is_empty()
        && p.models
            .values()
            .all(|m| matches!(m.billing, Billing::Subscription));
    let mut out = ProviderUsage {
        label: u
            .label
            .clone()
            .or_else(|| p.label.clone())
            .unwrap_or_else(|| u.provider.clone()),
        provider: u.provider.clone(),
        subscription,
        declared: p.usage.is_some(),
        budget: p.budget.as_ref().map(|b| Budget {
            usd: b.usd,
            period: match b.period {
                code_config::BudgetPeriod::Day => "day",
                code_config::BudgetPeriod::Week => "week",
                code_config::BudgetPeriod::Month => "month",
            }
            .into(),
            spent: Some(u.spent_usd),
            resets: u.budget.as_ref().and_then(|l| l.resets_at.clone()),
        }),
        error: u.error.clone(),
        stale: u.stale,
        refreshing: u.refreshing,
        unknown_cost_calls: u.unknown_cost_calls,
        warnings: u.warnings.clone(),
        ..ProviderUsage::default()
    };
    match u.document.as_ref().map(serde_json::to_value) {
        Some(Ok(doc)) => match usage::parse_doc(&doc) {
            Ok((account, limits, credits, notes)) => {
                out.account = account;
                out.limits = limits;
                out.credits = credits;
                out.notes = notes;
                out.fetched = u
                    .fetched_at
                    .as_deref()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&chrono::Local));
            }
            Err(e) => out.error = Some(e),
        },
        _ => {
            let account = std::fs::read_to_string(
                dirs.cache
                    .join("discover")
                    .join(format!("{}.json", u.provider)),
            )
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .map(|v| v["account"].clone())
            .filter(|a| !a.is_null());
            if let Some(account) = account {
                out.limits = usage::account_limits(&account);
                out.account = usage::text(&account);
            }
            if out.declared && out.error.is_none() && !out.refreshing {
                out.error = Some("no usage report yet".into());
            }
        }
    }
    // The account often repeats the provider label the header already shows.
    if let Some(a) = out.account.take() {
        let rest: Vec<&str> = a.split(" · ").filter(|p| *p != out.label).collect();
        out.account = (!rest.is_empty()).then(|| rest.join(" · "));
    }
    out
}

fn cost_of(t: &code_engine::schema::SessionTotals) -> String {
    fmt_cost(t.known_cost_usd, t.known_cost_calls, t.unknown_cost_calls)
}

/// "3m", "2h", "5d" since an RFC3339 timestamp.
pub fn age(ts: &str) -> String {
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return "?".into();
    };
    let secs = (chrono::Utc::now() - then.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
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

/// `/exclusive` (D67): one model for the whole tree.
#[derive(Clone, Debug)]
pub struct Exclusive {
    pub model: String,
    /// None: no safety model (manual and yolo only).
    pub safety: Option<String>,
    /// Also block web tools and remote (http/sse) MCP servers.
    pub block_remote: bool,
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
    /// send so an unused session is never written (D44).
    pending_mode: Option<PermissionMode>,
    /// Shutdowns of sessions left by a switch; awaited before exit.
    retired: Vec<tokio::task::JoinHandle<()>>,
    /// `--model` given with a resume/continue/fork: applied by the UI after
    /// the cache-aware confirmation (D49), not at open.
    pub deferred_model: Option<String>,
    /// A background discovery run: its providers and the reloaded store.
    discovery: Option<Discovery>,
    model_selection: Option<std_mpsc::Receiver<Result<String, String>>>,
    /// Opting a switched-to session into private terminals failed (D144).
    pub terminal_issue: Option<String>,
}

/// Token and cost totals of one agent's own calls.
#[derive(Clone, Debug, Default)]
pub struct Totals {
    pub calls: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub known_cost: f64,
    pub known_calls: u64,
    pub unknown_calls: u64,
}

impl Totals {
    pub fn add(&mut self, o: &Totals) {
        self.calls += o.calls;
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.known_cost += o.known_cost;
        self.known_calls += o.known_calls;
        self.unknown_calls += o.unknown_calls;
    }
    pub fn cost(&self) -> String {
        fmt_cost(self.known_cost, self.known_calls, self.unknown_calls)
    }
}

/// One configured MCP server (`/mcp`).
#[derive(Clone, Debug)]
pub struct McpView {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    /// disconnected, connecting, connected or error.
    pub state: String,
    pub tools: usize,
    pub error: Option<String>,
}

/// The context meter at open: (used, window, estimated).
#[derive(Clone, Copy, Debug, Default)]
pub struct ResumeContext {
    pub used: u64,
    pub window: Option<u64>,
    pub estimate: bool,
    /// The provider reports no context (see [`ModelInfo::context_unknown`]).
    pub unknown: bool,
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
        // cache-aware confirmation (D49).
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
        // D144: this frontend renders private terminals; opt in before
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
        // The old session's jobs and children stop (D38); its lock is released after.
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
        // D103: only the current context and the one before it are read and
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

    /// Read one older context in the background (D103 scroll-back).
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

    /// Command providers whose discovery cache is missing, past its TTL or
    /// older than the provider file.
    fn discovery_expired(&self) -> BTreeSet<String> {
        let snap = self.store.snapshot();
        let dirs = &self.directories;
        snap.providers
            .iter()
            .filter(|(_, p)| p.enabled)
            .filter_map(|(id, p)| {
                let d = p.command.as_ref()?.discover.as_ref()?;
                let cache = dirs.cache.join("discover").join(format!("{id}.json"));
                let cached = std::fs::metadata(&cache).and_then(|m| m.modified()).ok();
                let config =
                    std::fs::metadata(dirs.config.join("providers").join(format!("{id}.json")))
                        .and_then(|m| m.modified())
                        .ok();
                let fresh = cached.is_some_and(|c| {
                    c.elapsed()
                        .is_ok_and(|age| age.as_secs() < d.ttl_secs.unwrap_or(86_400))
                        && config.is_none_or(|cfg| cfg <= c)
                });
                // A cached failure retries once its negative-cache entry expires.
                let retry = snap.discovery_errors.get(id).is_some_and(|e| {
                    e.cached
                        && std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .is_ok_and(|now| now.as_secs() >= e.retry_at)
                });
                (!fresh || retry).then(|| id.clone())
            })
            .collect()
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
            self.discovery_expired()
        };
        if targets.is_empty() {
            return false;
        }
        let mut store = self.store.clone();
        let list = targets.clone();
        let (tx, rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            // One enrichment pass forces exactly these providers (engine-j);
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
    /// imports every attachment into the session `assets/` at send (D79).
    pub fn attachment_dir(&self) -> PathBuf {
        self.directories.cache.join("attachments")
    }

    /// Providers being discovered now: a `/model` refresh, or the engine's
    /// own background refresh (its expired providers).
    pub fn discovering(&self) -> BTreeSet<String> {
        match &self.discovery {
            Some((s, _)) => s.clone(),
            None if self.session.discovery_running() => self.discovery_expired(),
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
    /// command now (D85); without `force` the cache's TTL decides.
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
                Some(usage_of(&self.directories, p, u))
            })
            .collect()
    }

    pub fn catalog(&self) -> Catalog {
        let snap = self.store.snapshot();
        let mut models = Vec::new();
        let toggles = ModelToggles::default();
        for (pid, provider) in &snap.providers {
            for (mid, m) in &provider.models {
                if self
                    .session
                    .exclusive()
                    .as_ref()
                    .is_some_and(|ex| ex.model != format!("{pid}/{mid}"))
                {
                    continue;
                }
                let text_only =
                    m.capabilities.output.is_empty() || m.capabilities.output == [Modality::Text];
                models.push(ModelInfo {
                    id: format!("{pid}/{mid}"),
                    provider: pid.clone(),
                    provider_label: provider.label.clone().unwrap_or_else(|| pid.clone()),
                    label: m.label.clone().unwrap_or_else(|| mid.clone()),
                    description: m.description.clone(),
                    context_window: m.context_window,
                    max_output: m.max_output_tokens,
                    efforts: m.reasoning.efforts.clone(),
                    default_effort: m.reasoning.default.clone(),
                    input: m
                        .capabilities
                        .input
                        .iter()
                        .map(modality)
                        .chain(m.capabilities.file_input.then(|| "file".to_string()))
                        .collect(),
                    output: m.capabilities.output.iter().map(modality).collect(),
                    tools: m.capabilities.tools,
                    benchmarks: m.benchmarks.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                    cost: m.cost.as_ref().map(|c| CostRates {
                        input: c.input_per_m_tok,
                        output: c.output_per_m_tok,
                        cache_read: c.cache_read_per_m_tok,
                        cache_write: c.cache_write_per_m_tok,
                    }),
                    provider_enabled: provider.enabled,
                    subagent_capable: code_config::subagent_eligible(
                        pid, mid, provider, m, &toggles,
                    ),
                    chat_capable: text_only || m.capabilities.output.contains(&Modality::Text),
                    subscription: matches!(m.billing, Billing::Subscription),
                    context_unknown: m.kind.unwrap_or(provider.kind)
                        == code_config::ProviderType::Command
                        && provider
                            .command
                            .as_ref()
                            .is_some_and(|c| c.history.as_deref() == Some("latest")),
                });
            }
        }
        let mut benchmarks: Vec<Benchmark> = snap
            .config
            .benchmarks
            .iter()
            .map(|(k, b)| Benchmark {
                key: k.clone(),
                label: b.label.clone(),
                higher_is_better: b.higher_is_better,
            })
            .collect();
        // Benchmarks used on models but missing from the registry default to higher-is-better.
        for m in &models {
            for (k, _) in &m.benchmarks {
                if !benchmarks.iter().any(|b| &b.key == k) {
                    benchmarks.push(Benchmark {
                        key: k.clone(),
                        label: k.clone(),
                        higher_is_better: true,
                    });
                }
            }
        }
        let config_var = ["COCKPIT_CODE_HOME", "XDG_CONFIG_HOME"]
            .into_iter()
            .find(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()));
        // Discovery failures are structured per provider (engine-j); model
        // warnings ("{id}/{model}: Warning: …") are in the model description
        // already. Anything else is a skipped file.
        let providers: Vec<ProviderView> = snap
            .providers
            .iter()
            .map(|(id, p)| {
                let failure = snap.discovery_errors.get(id);
                ProviderView {
                    id: id.clone(),
                    label: p.label.clone().unwrap_or_else(|| id.clone()),
                    enabled: p.enabled,
                    discovers: p
                        .command
                        .as_ref()
                        .and_then(|c| c.discover.as_ref())
                        .is_some(),
                    warning: failure.map(|e| e.message.clone()),
                    warning_detail: failure.map(|e| {
                        let kind = serde_json::to_value(e.kind)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default();
                        let mut parts = vec![kind];
                        if e.cached {
                            parts.push("cached".into());
                            if let Some(t) = chrono::DateTime::from_timestamp(e.retry_at as i64, 0)
                            {
                                parts.push(format!(
                                    "retry {}",
                                    t.with_timezone(&chrono::Local).format("%H:%M")
                                ));
                            }
                        }
                        parts.join(" · ")
                    }),
                }
            })
            .collect();
        let mut errors = vec![];
        for e in &snap.errors {
            let (head, rest) = e.split_once(": ").unwrap_or(("", e));
            let model_warning = head.split_once('/').is_some_and(|(p, m)| {
                snap.providers
                    .get(p)
                    .is_some_and(|p| p.models.contains_key(m))
            }) && rest.starts_with("Warning:");
            let discovery = snap.discovery_errors.contains_key(head)
                || (providers.iter().any(|p| p.id == head && p.discovers)
                    && rest.ends_with("using static models"));
            if !model_warning && !discovery {
                errors.push(e.clone());
            }
        }
        Catalog {
            models,
            providers,
            benchmarks,
            theme: snap.config.theme.clone(),
            mouse: snap.config.mouse,
            attention: snap.config.attention.clone(),
            vim: snap.config.vim,
            paste_collapse_lines: snap.config.paste_collapse_lines,
            queue_default: QueueClass::from_config(snap.config.queue_default),
            config_dir: self.directories.config.clone(),
            config_var,
            errors,
        }
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
    // ---- skills as slash commands (D109) ----------------------------------

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

    // ---- engine-l: idle compaction, reviewer guidance, discovery ---------

    /// The D100 idle hook, called about once a second. `busy` (typing, an
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
    // ---- engine-o: scopes, subscriptions, guidance; prune; D122 ----------

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
                    &id[..id.len().min(8)],
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
    /// D121 selections, chronological, optionally for one agent.
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
    /// D122: tell `agent` that the user ran a local command (no output).
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

    // ---- criteria and /goal (D88–D97, D110) -------------------------------

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
        let t = self.session.goal_review_totals();
        Totals {
            calls: t.calls,
            input: t.input_tokens,
            output: t.output_tokens,
            cache_read: t.cache_read_tokens,
            cache_write: t.cache_write_tokens,
            known_cost: t.known_cost_usd,
            known_calls: t.known_cost_calls,
            unknown_calls: t.unknown_cost_calls,
        }
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
        if items.iter().all(|q| q.id.is_none()) {
            let active = self.session.is_active();
            self.session
                .answer(id, answers_text(items, answers))
                .map_err(err)?;
            return Ok(!active && self.session.is_active());
        }
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
    /// Attachment chips (D79) travel as `<attachment>` tags; the engine
    /// imports complete tags into the session `assets/` and strips them from
    /// the text (engine-j). Text files are inlined for every model; binaries
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
    /// Startup health check of the enabled MCP servers (D69): (server,
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
    /// The session title (D55): a user title, else the automatic one.
    pub fn title(&self) -> Option<String> {
        Some(self.session.title()).filter(|t| !t.trim().is_empty())
    }
    /// Set a user title (persisted; stops automatic titling, D55). The
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
        let t = self.session.agent_totals(id);
        Totals {
            calls: t.calls,
            input: t.input_tokens,
            output: t.output_tokens,
            cache_read: t.cache_read_tokens,
            cache_write: t.cache_write_tokens,
            known_cost: t.known_cost_usd,
            known_calls: t.known_cost_calls,
            unknown_calls: t.unknown_cost_calls,
        }
    }
    /// The session safety model (D45), persisted in `settings.json`; new
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
    /// The context meter for a session just opened (D48): the last root
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
        // A provider-reported meter (m7) wins; without one, an unknown
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
    /// Steer a running subagent; the text reaches it at its next turn boundary.
    #[allow(dead_code)] // Compatibility seam for frontends steering a selected child.
    pub fn steer(&self, agent: &str, text: String) -> Result<(), String> {
        self.session.steer(agent, text).map_err(err)
    }
    // ---- queued messages (D73, D74) ---------------------------------------

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
    pub fn requeue(&mut self, item: Queued, _at: usize) {
        if let Err(error) = self.queue_message(&item.agent, item.text, item.class) {
            eprintln!("could not requeue message: {error}");
        }
    }
    pub fn reclassify(&mut self, id: u64, class: QueueClass) -> Result<(), String> {
        self.session.reclassify(id, class.config()).map_err(err)
    }
    pub fn unqueue(&mut self, id: u64) -> Option<(Queued, usize)> {
        let (item, at) = self.session.unqueue(id).ok()??;
        Some((queued_view(item), at))
    }
    pub fn queued(&self, agent: &str) -> Vec<Queued> {
        self.session
            .queued_messages(agent)
            .into_iter()
            .map(queued_view)
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

fn queued_view(item: code_engine::QueuedMessage) -> Queued {
    Queued {
        id: item.id,
        agent: item.agent,
        text: item.text,
        class: QueueClass::from_config(item.class),
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

/// Recalled and replayed messages use immutable imported attachment copies.
fn user_wire(text: &str, attachments: &[code_engine::schema::Attachment], assets: &Path) -> String {
    let mut text = strip_ctx(text);
    for attachment in attachments {
        let path = assets.join(&attachment.path);
        let path = path.to_string_lossy();
        let kind = crate::attach::kind_of(std::path::Path::new(path.as_ref()));
        let id = crate::paste::fresh_id(&path);
        if !text.is_empty() && !text.ends_with(char::is_whitespace) {
            text.push(' ');
        }
        text.push_str(&crate::paste::attach_wire(&id, kind, &path));
    }
    text
}

fn replay_items(agent: &str, envelopes: &[EventEnvelope], assets: &Path) -> Vec<ReplayItem> {
    let mut out = vec![];
    let mut spoke = false;
    for e in envelopes {
        match &e.event {
            HistoryEvent::User { text, attachments } => {
                spoke = true;
                let text = user_wire(text, attachments, assets);
                out.push(ReplayItem::User(text, e.ts.clone()));
            }
            HistoryEvent::Assistant {
                text,
                reasoning,
                tool_calls,
                ..
            } => {
                spoke = true;
                let reasoning = reasoning
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() || !reasoning.trim().is_empty() {
                    out.push(ReplayItem::Assistant {
                        text: text.clone(),
                        reasoning,
                        ts: e.ts.clone(),
                    });
                }
                for c in tool_calls {
                    out.push(ReplayItem::ToolCall {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        args: c.arguments.clone(),
                    });
                }
            }
            HistoryEvent::ScriptCall {
                parent_call_id,
                call,
                result,
            } => out.push(ReplayItem::ScriptCall {
                parent: parent_call_id.clone(),
                call: call.clone(),
                result: result.clone().map(|mut r| {
                    r.output = strip_guidance(strip_ctx(&r.output));
                    r
                }),
            }),
            HistoryEvent::ToolResult {
                id,
                output,
                is_error,
                details,
                ..
            } => out.push(ReplayItem::ToolResult {
                id: id.clone(),
                output: strip_guidance(strip_ctx(output)),
                is_error: *is_error,
                details: details.clone(),
            }),
            HistoryEvent::TurnError { error } => {
                out.push(ReplayItem::Error(match error_kind(&error.kind) {
                    "error" => error.message.clone(),
                    k => format!("{k}: {}", error.message),
                }))
            }
            // The leading summary of a context; a trailing one duplicates the next context's.
            HistoryEvent::Compaction { summary } if !spoke => {
                out.push(ReplayItem::Compacted(summary_text(summary)))
            }
            HistoryEvent::Answer { text, .. } => {
                out.push(ReplayItem::Notice(format!("answered: {text}")))
            }
            HistoryEvent::ModelSwitch { from, to, reason } => out.push(ReplayItem::Switched {
                from: from.clone(),
                to: to.clone(),
                reason: reason.clone(),
            }),
            HistoryEvent::CallUpdate { call, reason } => {
                if let Some(why) = reason
                    .strip_prefix("moved to background (")
                    .and_then(|r| r.strip_suffix(')'))
                {
                    out.push(ReplayItem::Moved {
                        id: call.id.clone(),
                        reason: why.to_string(),
                    })
                }
            }
            HistoryEvent::QuestionForkAnswered { id, summary, .. } => {
                out.push(ReplayItem::ForkAnswered {
                    id: id.clone(),
                    summary: summary.clone(),
                })
            }
            HistoryEvent::Message { from, to, text } if to == agent && from != "jobs" => {
                out.push(ReplayItem::Message {
                    from: from.clone(),
                    to: to.clone(),
                    text: strip_ctx(text),
                })
            }
            HistoryEvent::JobUpdate { job } if !job.running() => {
                out.push(ReplayItem::Job(job_view(job.clone())))
            }
            HistoryEvent::ScopeRequest { request } => {
                out.push(ReplayItem::Scope(scope_view(request.clone())))
            }
            HistoryEvent::SubscriptionNotice {
                changes, overflow, ..
            } => out.push(ReplayItem::Subscription {
                changes: change_views(changes.clone()),
                overflow: *overflow,
            }),
            HistoryEvent::GuidanceSelected { selection } => {
                out.push(ReplayItem::Guidance(guidance_sel(selection.clone())))
            }
            HistoryEvent::Pruned { report, .. } => {
                out.push(ReplayItem::Pruned(prune_view(report.clone())))
            }
            HistoryEvent::LocalCommand {
                command,
                exit_code,
                duration_ms,
                output_bytes,
            } => out.push(ReplayItem::LocalRun(LocalRun {
                command: command.clone(),
                exit_code: *exit_code,
                duration_ms: *duration_ms,
                output_bytes: *output_bytes,
            })),
            other => {
                if let Some((pct, used, window)) = tree::compact_suggestion(other) {
                    out.push(ReplayItem::CompactSuggested(pct, used, window));
                } else if let Some(row) = tree::tree_row(other) {
                    out.push(ReplayItem::Tree(row));
                }
            }
        }
    }
    out
}

/// The label an error kind gets in front of its message ("error" gets none).
fn error_kind(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Auth => "auth",
        ErrorKind::Network => "network",
        ErrorKind::Billing => "billing",
        ErrorKind::RateLimit => "rate limit",
        ErrorKind::ContextOverflow => "context",
        ErrorKind::BadRequest => "error",
        ErrorKind::Unavailable => "unavailable",
        ErrorKind::LockConflict => "locked",
        ErrorKind::ReadOnly => "read-only",
    }
}

/// Tool output without appended D121 guidance notices (a
/// `<system-message>` wrapping `<guidance-selection>`): the transcript shows
/// them as their own row.
pub fn strip_guidance(s: String) -> String {
    const OPEN: &str = "<system-message id=\"";
    if !s.contains("<guidance-selection id=") {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(at) = rest.find(OPEN) {
        let after = &rest[at + OPEN.len()..];
        let Some(q) = after.find("\">") else { break };
        let body = after[q + 2..].trim_start();
        let close = format!("</system-message id=\"{}\">", &after[..q]);
        match after.find(&close) {
            Some(end) if body.starts_with("<guidance-selection id=") => {
                out.push_str(rest[..at].trim_end_matches('\n'));
                rest = &after[end + close.len()..];
            }
            _ => {
                out.push_str(&rest[..at + OPEN.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Text without D99 `<ctxPercentage id="R">N</ctxPercentage id="R">` tags:
/// they are for the model; the status line shows context use.
fn strip_ctx(s: &str) -> String {
    const OPEN: &str = "<ctxPercentage id=\"";
    if !s.contains(OPEN) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(OPEN) {
        let after = &rest[at + OPEN.len()..];
        let Some(q) = after.find('"') else { break };
        let close = format!("</ctxPercentage id=\"{}\">", &after[..q]);
        let Some(end) = after.find(&close) else { break };
        out.push_str(rest[..at].trim_end_matches([' ', '\n']));
        rest = &after[end + close.len()..];
    }
    out.push_str(rest);
    out
}

/// A compaction summary without its injection wrapper: the old context's
/// copy and the new context's head carry different random ids (D52), so
/// the bare text identifies the summary and is what is shown.
fn summary_text(s: &str) -> String {
    let t = s.trim();
    if let Some(after) = t.strip_prefix("<system-message id=\"")
        && let Some(q) = after.find("\">")
    {
        let close = format!("</system-message id=\"{}\">", &after[..q]);
        if let Some(end) = after.find(&close) {
            return after[q + 2..end].trim().to_string();
        }
    }
    t.to_string()
}

/// Translate the long-lived session subscription into frontend events.
fn subscribe(session: &Session, assets: PathBuf) -> EventRx {
    let mut rx = session.subscribe();
    let health = session.clone();
    tokio::spawn(async move {
        health.mcp_health_check().await;
    });
    let (tx, out) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut compacted_turn = None;
        while let Some(event) = rx.recv().await {
            if let EngineEvent::Committed { envelope } = &event
                && envelope.agent_id == "root"
            {
                match &envelope.event {
                    HistoryEvent::Compaction { .. } => {
                        compacted_turn = Some(envelope.turn_id.clone());
                    }
                    HistoryEvent::User { text, attachments }
                        if compacted_turn.as_ref() != Some(&envelope.turn_id) =>
                    {
                        let text = user_wire(text, attachments, &assets);
                        if tx.send(UiEvent::UserRecorded(text)).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(event) = translate(event)
                && tx.send(event).is_err()
            {
                break;
            }
        }
    });
    out
}

fn translate(event: EngineEvent) -> Option<UiEvent> {
    Some(match event {
        EngineEvent::DecisionUpdated { open_count, .. } => UiEvent::Decision { open_count },
        EngineEvent::GuidanceSelected { selection } => UiEvent::Guidance(guidance_sel(selection)),
        EngineEvent::SubscriptionNotice {
            agent_id,
            changes,
            overflow,
        } => UiEvent::Subscription {
            agent: agent_id,
            changes: change_views(changes),
            overflow,
        },
        EngineEvent::ScopeRequestUpdate { request } => UiEvent::Scope(scope_view(request)),
        EngineEvent::ForkChat {
            fork_id,
            kind,
            text,
            messages,
            tool_call,
            tool_result,
            ..
        } => UiEvent::ForkChat {
            fork: fork_id,
            kind,
            text,
            messages,
            call: tool_call,
            result: tool_result,
        },
        EngineEvent::ReviewerGuidanceTrustRequest { request } => {
            UiEvent::GuidanceTrust(guidance_view(request))
        }
        EngineEvent::RootCompactionSuggested { used, window, .. } => {
            UiEvent::RootCompactionSuggested { used, window }
        }
        EngineEvent::IdleCompacted => UiEvent::IdleCompacted,
        EngineEvent::DiscoveryUpdated { .. } => UiEvent::DiscoveryUpdated,
        EngineEvent::GoalDraft { contract } => UiEvent::GoalDraft(goal::contract(&contract)),
        EngineEvent::GoalUpdate { .. } => UiEvent::GoalUpdate,
        EngineEvent::ProposalRequest { agent_id, proposal } => UiEvent::ProposalRequest {
            agent: agent_id,
            proposal: goal::proposal(&proposal),
        },
        EngineEvent::RoadblockRequest {
            agent_id,
            roadblock,
        } => UiEvent::RoadblockRequest {
            agent: agent_id,
            roadblock: goal::roadblock(&roadblock),
        },
        EngineEvent::Text { delta } => UiEvent::Text(delta),
        EngineEvent::Reasoning { delta } => UiEvent::Reasoning(delta),
        EngineEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } => UiEvent::ToolDelta {
            index,
            id,
            name,
            args: arguments,
        },
        // Children's reviews arrive wrapped in AgentEvent (routed below).
        EngineEvent::SafetyReview {
            call_id,
            model,
            phase,
            verdict,
            reason,
            ..
        } => UiEvent::SafetyReview {
            call_id,
            model,
            result: (phase != "started").then(|| {
                let verdict = verdict.unwrap_or_else(|| "error".into());
                (verdict == "safe", verdict, reason.unwrap_or_default())
            }),
        },
        EngineEvent::ToolStart { call } => UiEvent::ToolStart {
            id: call.id,
            name: call.name,
            args: call.arguments,
        },
        EngineEvent::ToolEnd {
            id,
            output,
            is_error,
            details,
            ..
        } => UiEvent::ToolEnd {
            id,
            output: strip_guidance(strip_ctx(&output)),
            is_error,
            details,
        },
        EngineEvent::MovedToBackground {
            call_id,
            job_id,
            reason,
        } => UiEvent::MovedToBackground {
            call_id,
            job: job_id,
            reason,
        },
        EngineEvent::QueueChanged => UiEvent::QueueChanged,
        EngineEvent::QueuedMessageDelivered { message } => UiEvent::Message {
            from: "user".into(),
            to: message.agent,
            text: strip_ctx(&message.text),
        },
        EngineEvent::Todo { items } => UiEvent::Todo(todo_items(&items)),
        EngineEvent::Usage { usage } => {
            if usage.partial {
                return None;
            }
            UiEvent::Usage(usage_view(&usage))
        }
        EngineEvent::Pruned { report } => UiEvent::Pruned(prune_view(report)),
        EngineEvent::Reconnecting { attempt, delay_ms } => {
            UiEvent::Reconnecting { attempt, delay_ms }
        }
        EngineEvent::ModelSwitched { from, to, reason } => {
            UiEvent::ModelSwitched { from, to, reason }
        }
        EngineEvent::Retry {
            attempt,
            delay_ms,
            reason,
        } => UiEvent::Retry {
            attempt,
            delay_ms,
            reason,
        },
        EngineEvent::StreamReset { .. } => UiEvent::StreamReset,
        EngineEvent::Error { error } => UiEvent::Error {
            kind: error_kind(&error.kind),
            message: error.message,
        },
        EngineEvent::Context { meter } => UiEvent::Context {
            used: meter.used,
            window: meter.window,
            reserved: meter.output_reservation,
            warning: meter.warning,
            known: meter.source != "unknown",
            estimate: meter.source.contains("tokenizer"),
        },
        EngineEvent::ApprovalRequest { request } => UiEvent::Approval(ApprovalView {
            call_id: request.call_id,
            parent_call_id: request.parent_call_id,
            id: request.id,
            agent: request.agent_id,
            job: request.job_id,
            tool: request.tool,
            breadcrumb: request.breadcrumb,
            target: request.target,
            diff: request.diff,
            command: request.command,
            reason: request.reason,
            grant_key: request.grant_key,
            verdict: request.sidecar_verdict.map(|v| {
                let why = request
                    .sidecar_reason
                    .into_iter()
                    .chain(request.sidecar_hint)
                    .collect::<Vec<_>>()
                    .join(" · ");
                (v, why)
            }),
        }),
        EngineEvent::Question { question } => {
            if question.recipient.is_some() {
                return None;
            }
            UiEvent::Question(QuestionView {
                questions: structured_items(&question),
                id: question.id,
                asker: question.asker,
                breadcrumb: question.breadcrumb,
                note: question.note,
                blocking: question.blocking,
            })
        }
        EngineEvent::SessionClaimed { message, .. } => UiEvent::Claimed(message),
        EngineEvent::PartialCompletion { native_reason, .. } if native_reason == "interrupted" => {
            UiEvent::Interrupted
        }
        EngineEvent::PartialCompletion { status, text, .. } => UiEvent::Partial {
            status: status.as_str().to_string(),
            text,
        },
        EngineEvent::Committed { envelope } => match envelope.event {
            HistoryEvent::Answer { id, .. }
            | HistoryEvent::QuestionsAnswered { id, .. }
            | HistoryEvent::QuestionDiscussed { id } => UiEvent::Answered(id),
            HistoryEvent::QuestionForkAnswered { id, summary, .. } => UiEvent::ForkAnswered {
                id,
                asker: envelope.agent_id,
                summary,
            },
            HistoryEvent::Goal { .. } => UiEvent::ContractChanged(envelope.agent_id),
            HistoryEvent::Compaction { summary } if envelope.agent_id == "root" => {
                UiEvent::Compacted(summary_text(&summary))
            }
            HistoryEvent::Compaction { summary } => UiEvent::Child {
                id: envelope.agent_id,
                event: Box::new(UiEvent::Compacted(summary_text(&summary))),
            },
            HistoryEvent::SettingChange { .. } => UiEvent::SettingsChanged,
            ref other => UiEvent::Tree {
                row: tree::tree_row(other)?,
                agent: envelope.agent_id,
            },
        },
        EngineEvent::TurnEnd { stop_reason } => UiEvent::TurnEnd { stop_reason },
        EngineEvent::AgentStarted {
            agent,
            execution,
            task_call_id,
            ..
        } => UiEvent::AgentStarted(AgentView {
            id: agent.agent_id,
            parent: agent.parent_id.unwrap_or_else(|| "root".into()),
            role: agent.agent,
            model: agent.model,
            status: agent.status,
            task: execution.task,
            background: execution.background,
            created: agent.created_at,
            call_id: task_call_id,
        }),
        EngineEvent::AgentStatus {
            agent_id, status, ..
        } => UiEvent::AgentStatus {
            id: agent_id,
            status,
        },
        EngineEvent::AgentFinished {
            agent_id, status, ..
        } => UiEvent::AgentFinished {
            id: agent_id,
            status,
        },
        // Terminal metadata is top-level whoever asked.
        EngineEvent::AgentEvent { event, .. }
            if matches!(*event, EngineEvent::TerminalChanged { .. }) =>
        {
            return translate(*event);
        }
        EngineEvent::AgentEvent {
            agent_id, event, ..
        } => UiEvent::Child {
            id: agent_id,
            event: Box::new(translate(*event)?),
        },
        // A partial report renders from its durable event (replaced in place).
        EngineEvent::AgentMessage { text, .. } if text.starts_with("Partial report: ") => {
            return None;
        }
        EngineEvent::AgentMessage { from, to, text, .. } => UiEvent::Message {
            from,
            to,
            text: strip_ctx(&text),
        },
        EngineEvent::Job { job } => UiEvent::Job(job_view(job)),
        EngineEvent::TerminalChanged { terminal } => terminal::translate(terminal),
        // Script progress gets its own rows in a later TUI lane.
        EngineEvent::ScriptCall {
            job_id,
            call,
            parent_call_id,
            result,
            status,
            ..
        } => UiEvent::ScriptCall {
            job: job_id,
            parent: parent_call_id,
            call,
            result: result.map(|mut r| {
                r.output = strip_guidance(strip_ctx(&r.output));
                r
            }),
            status,
        },
        EngineEvent::TitleChanged { title, source } => UiEvent::TitleChanged {
            title,
            auto: matches!(source, code_engine::schema::TitleSource::Auto),
        },
        EngineEvent::HiddenForkFinished { outcome } => UiEvent::HiddenFork {
            purpose: outcome.purpose,
            ok: outcome.succeeded,
        },
    })
}
