//! The plain data the UI renders and sends back: no engine handles, only
//! values (and the few pure helpers that read them).
use super::goal::{ContractView, ProposalView, RoadblockView};
use super::terminal::TermInfo;
use super::tree::TreeRow;
use crate::usage::Limit;
use code_engine::ForkChatMessage;
use code_engine::schema::{ToolCall, ToolResultData, ToolResultDetails};
use serde_json::Value;
use std::path::PathBuf;
use tokio::sync::mpsc;

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
    /// The asker is parked until answered.
    pub blocking: bool,
    /// The structured questions of the call.
    pub questions: Vec<QItem>,
}

/// Question kinds.
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

/// One question of a `question` call.
#[derive(Debug, Clone)]
pub struct QItem {
    pub id: Option<String>,
    pub text: String,
    pub kind: QKind,
    pub options: Vec<QOption>,
    pub multi: bool,
}

/// The answer to one question: chosen labels plus other text, a
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

/// Questions of a call, preserving the engine's structured item IDs.
/// The answers as one text block for a transcript row.
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

/// Whether an agent status means the agent loop is still alive.
pub fn alive(status: &str) -> bool {
    matches!(status, "running" | "waiting" | "compacting") || status.starts_with("queued")
}

/// An agent status for display, including `queued` (waiting for a slot on a
/// model or provider with `maxConcurrentContexts`); a queue position shows
/// as `queued (2nd)`.
///
/// The engine emits `queued:N`.
pub fn status_text(status: &str) -> String {
    let Some(rest) = status.strip_prefix("queued:") else {
        return status.to_string();
    };
    let n: Option<usize> = rest.parse().ok();
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

/// A skill the user can invoke as `/<name>`.
#[derive(Debug, Clone)]
pub struct SkillView {
    pub name: String,
    pub description: String,
    /// `disable-model-invocation: true`: hidden from the model ("you only").
    pub user_only: bool,
    /// A project skill that overrides a global one of the same name.
    pub shadows_global: bool,
}

/// Project reviewer guidance awaiting trust: the exact content.
#[derive(Debug, Clone)]
pub struct GuidanceView {
    pub id: String,
    pub path: PathBuf,
    pub sha256: String,
    pub content: String,
    /// An unapproved change was restored; `content` is the quarantined proposal.
    pub tampered: bool,
    /// The proposal is to delete the file.
    pub deleted: bool,
    /// A project guidance library file, not reviewer guidance.
    pub library: bool,
}

/// An out-of-scope write/edit routed to the scope's owner.
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

/// The owner's answer to a scope request.
#[derive(Debug, Clone)]
pub enum ScopeChoice {
    Apply,
    Reject(String),
    Grant(Vec<PathBuf>),
}

/// One changed path of a subscription notice.
#[derive(Debug, Clone)]
pub struct ChangeView {
    pub path: PathBuf,
    /// The agent that wrote it; None is the user's editor or unknown.
    pub source: Option<String>,
    /// The engine's notice text (a bounded diff, or "changed; re-read").
    pub text: String,
}

/// A guidance selection: what was injected after a write, and why.
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

/// A `/prune` preview or result.
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

/// The facts of a `!cmd` run, as the agent was told (never the output).
#[derive(Debug, Clone)]
pub struct LocalRun {
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub output_bytes: u64,
}

/// Frontend view of one engine event.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// Terminal metadata (never its bytes).
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
        /// The containing script call.
        parent: Option<String>,
        call: ToolCall,
        result: Option<ToolResultData>,
        status: String,
    },
    /// A sidecar safety review of a call started (`result` None) or
    /// finished: (safe, verdict, reason).
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
    /// background as job `job`.
    MovedToBackground {
        call_id: String,
        job: String,
        /// `steer` or `interrupt`.
        reason: String,
    },
    /// An interrupt stopped the stream: the partial text is kept,
    /// tentative tool calls are dropped.
    Interrupted,
    /// The engine's queue changed (delivered, reclassified); redraw.
    QueueChanged,
    /// A durably recorded root user message for composer recall.
    UserRecorded(String),
    Usage(Usage),
    Retry {
        attempt: usize,
        delay_ms: u64,
        reason: Option<String>,
    },
    StreamReset,
    /// The network dropped: the engine probes and retries by itself.
    Reconnecting {
        attempt: usize,
        delay_ms: u64,
    },
    /// A backup model took over for the rest of this turn.
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
    /// Project reviewer guidance needs the user's trust.
    GuidanceTrust(GuidanceView),
    /// A scope request changed state (upsert by id).
    Scope(ScopeView),
    /// Subscribed paths changed (delivered to `agent`).
    Subscription {
        agent: String,
        changes: Vec<ChangeView>,
        overflow: bool,
    },
    /// Guidance was selected (or nothing / a failure) after a write.
    Guidance(GuidanceSel),
    /// `/prune` finished.
    Pruned(PruneView),
    /// Root was compacted while the tree was idle.
    IdleCompacted,
    RootCompactionSuggested {
        used: u64,
        window: u64,
    },
    /// A durable tree row committed in `agent`'s journal.
    Tree {
        agent: String,
        row: TreeRow,
    },
    /// The ledger changed: the open count after the change.
    Decision {
        open_count: usize,
    },
    /// Background model discovery finished: re-read the catalog.
    DiscoveryUpdated,
    /// An agent's criteria contract changed.
    ContractChanged(String),
    /// `/goal` drafted a contract: open the approval dialog.
    GoalDraft(ContractView),
    /// The `/goal` run changed (disposition, phase, counters); read it
    /// back with `Engine::goal` (events can trail the state).
    GoalUpdate,
    /// An agent proposes a change to a user-owned criterion.
    ProposalRequest {
        agent: String,
        proposal: ProposalView,
    },
    /// A root roadblock awaits pivot, stop or override, in every mode.
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
    /// The session title changed, automatically or by `/rename`.
    TitleChanged {
        title: String,
        auto: bool,
    },
    /// A hidden fork (auto title) finished: its purpose and success.
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
    /// A question fork's chat event, routed by fork id into the
    /// fork-chat overlay: `opened` (with the saved `messages` as (role,
    /// text)), `user`, `assistant`, `tool` ("name: output"), `status`,
    /// `idle`, `closed` or `error`.
    ForkChat {
        fork: String,
        kind: String,
        text: String,
        messages: Vec<ForkChatMessage>,
        /// `tool_start` / `tool`: the structured call and result.
        call: Option<ToolCall>,
        result: Option<ToolResultData>,
    },
    /// A question fork answered question `id` of `asker`.
    ForkAnswered {
        id: String,
        asker: String,
        summary: String,
    },
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
    /// A live quota report of the call (`UsageRecord.quota`).
    pub quota: Option<Limit>,
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

/// How a queued user message is delivered.
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
}

/// A queued user message.
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
    /// A script host call: its containing script and result.
    ScriptCall {
        parent: Option<String>,
        call: ToolCall,
        result: Option<ToolResultData>,
    },
    Compacted(String),
    Notice(String),
    /// A backup model took over (from, to, reason).
    Switched {
        from: String,
        to: String,
        reason: String,
    },
    /// A persisted turn error: replayed as prominently as live.
    Error(String),
    /// A foreground call moved to the background: call id, reason.
    Moved {
        id: String,
        reason: String,
    },
    /// A question answered through its fork: question id, summary.
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
    /// A tree row.
    Tree(TreeRow),
    /// The root compaction suggestion (percent, used, window).
    CompactSuggested(u64, u64, u64),
}

/// A resumed transcript: earlier (compacted) context windows and the current one.
#[derive(Default)]
pub struct Replay {
    /// Context files older than `earlier`, not loaded.
    pub older: Vec<u32>,
    pub earlier: Vec<Vec<ReplayItem>>,
    pub current: Vec<ReplayItem>,
    pub usage: Vec<Usage>,
}

pub type EventRx = mpsc::UnboundedReceiver<UiEvent>;

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

/// `/exclusive`: one model for the whole tree.
#[derive(Clone, Debug)]
pub struct Exclusive {
    pub model: String,
    /// None: no safety model (manual and yolo only).
    pub safety: Option<String>,
    /// Also block web tools and remote (http/sse) MCP servers.
    pub block_remote: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_status_uses_current_colon_number_format() {
        for (n, suffix) in [
            (1, "st"),
            (2, "nd"),
            (3, "rd"),
            (4, "th"),
            (11, "th"),
            (12, "th"),
            (13, "th"),
            (21, "st"),
        ] {
            assert_eq!(
                status_text(&format!("queued:{n}")),
                format!("queued ({n}{suffix})")
            );
        }
        assert_eq!(status_text("running"), "running");
        assert_eq!(status_text("queued 2"), "queued 2");
    }
}
