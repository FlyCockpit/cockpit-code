//! Stable, provider-independent history and frontend protocol (JSONL-ready).
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default = "complete_call")]
    pub complete: bool,
}
impl ToolCall {
    pub(crate) fn replay_arguments(&self) -> Value {
        if self.arguments.is_object() {
            self.arguments.clone()
        } else {
            serde_json::json!({"_invalid_arguments": self.arguments})
        }
    }
}
fn complete_call() -> bool {
    true
}

/// File coordinates and fragments captured from the committed edit, for replay.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditHunk {
    pub start_line: usize,
    pub old_start_line: usize,
    pub old_text: String,
    pub new_text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEdit {
    pub path: std::path::PathBuf,
    pub hunks: Vec<EditHunk>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ToolResultDetails {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<FileEdit>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultData {
    pub output: String,
    pub is_error: bool,
    #[serde(flatten)]
    pub details: ToolResultDetails,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningBlock {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Opaque provider details are retained for signed/encrypted reasoning round trips.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<Value>,
}
/// Typed model input returned by a media tool; payload bytes stay in assets/.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MediaInput {
    Attachment { attachment: Attachment },
    Url { url: String, mime_type: String },
}
/// Immutable file in this session's assets directory. `path` is one relative filename.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub path: String,
    pub name: String,
    pub mime_type: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryEvent {
    /// Diagnostic only: never injected into the model's retained context.
    CompactionDraftRejected {
        draft: String,
        attempt: u32,
        reason: String,
        stop_reason: String,
        max_output_tokens: u64,
    },
    GuidanceSelected {
        selection: crate::guidance_selector::GuidanceSelection,
    },
    Subscriptions {
        paths: Vec<std::path::PathBuf>,
    },
    SubscriptionChanged {
        change: crate::subscriptions::SubscriptionChange,
    },
    SubscriptionOverflow,
    /// D122: the user ran `!cmd` locally while viewing this agent. Only the
    /// facts are recorded (never the output); the notice is delivered once,
    /// before the agent's next model request, as a system reminder.
    LocalCommand {
        command: String,
        exit_code: Option<i32>,
        duration_ms: u64,
        output_bytes: u64,
    },
    SubscriptionNotice {
        changes: Vec<crate::subscriptions::SubscriptionChange>,
        overflow: bool,
        text: String,
    },
    AgentScope {
        scope: crate::scopes::AgentScope,
    },
    ScopeRequest {
        request: crate::scopes::ScopeRequest,
    },
    ModelSwitch {
        from: String,
        to: String,
        reason: String,
    },
    ModelReminder {
        model: String,
        turn: u64,
    },
    /// Durable failure, including errors before an agent_request exists.
    TurnError {
        error: EngineError,
    },
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    Assistant {
        text: String,
        reasoning: Vec<ReasoningBlock>,
        tool_calls: Vec<ToolCall>,
        #[serde(default)]
        replay: Option<NativeReplay>,
        #[serde(default)]
        status: CompletionStatus,
        #[serde(default)]
        native_reason: String,
    },
    CallUpdate {
        call: ToolCall,
        reason: String,
    },
    /// A once-only replacement before this assistant block's first replay (D59).
    ThinkingUpdate {
        assistant: usize,
        #[serde(default)]
        call_id: Option<String>,
        reasoning: Vec<ReasoningBlock>,
    },
    OperationPrepared {
        operation: Value,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    Pruned {
        history: Vec<HistoryEvent>,
        report: crate::PruneReport,
    },
    HistoryReplace {
        history: Vec<HistoryEvent>,
        reason: String,
    },
    ToolMedia {
        id: String,
        inputs: Vec<MediaInput>,
    },
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
        #[serde(flatten)]
        details: ToolResultDetails,
    },
    /// Display-only host-call record; never serialized as a native model tool pair.
    ScriptCall {
        parent_call_id: Option<String>,
        call: ToolCall,
        result: Option<ToolResultData>,
    },
    SystemReminder {
        text: String,
    },
    PromptContext {
        context: Box<crate::context::FrozenContext>,
    },
    Reasoning {
        block: ReasoningBlock,
    },
    ToolCall {
        call: ToolCall,
    },
    Message {
        from: String,
        to: String,
        text: String,
    },
    Question {
        question: QuestionRecord,
    },
    Answer {
        id: String,
        text: String,
    },
    QuestionsAnswered {
        id: String,
        answers: Vec<QuestionAnswer>,
    },
    QuestionDiscussed {
        id: String,
    },
    DiscussionResumed {
        asker: String,
    },
    QuestionForkAnswered {
        id: String,
        answers: Vec<QuestionAnswer>,
        summary: String,
    },
    Approval {
        id: String,
        granted: bool,
        reason: String,
        #[serde(default)]
        verdict: Option<String>,
        #[serde(default)]
        hint: Option<String>,
    },
    Goal {
        contract: crate::goal::GoalContract,
    },
    GoalState {
        state: Box<crate::goal_controller::GoalRun>,
    },
    Job {
        id: String,
        status: String,
    },
    JobUpdate {
        job: JobRecord,
    },
    AgentStarted {
        agent: crate::sessions::AgentRecord,
        execution: AgentExecution,
        #[serde(default)]
        task_call_id: Option<String>,
    },
    AgentState {
        agent: crate::sessions::AgentRecord,
    },
    DecisionRecorded {
        record: crate::DecisionRecord,
    },
    DecisionReviewed {
        record: crate::DecisionRecord,
    },
    AgentReport {
        report: String,
        transcript: String,
    },
    AgentPartialReport {
        agent_id: String,
        text: String,
    },
    SubtreeGrowthDelivered {
        launches: Vec<String>,
    },
    AgentFinalizeRequested {
        target: String,
        agents: Vec<String>,
    },
    AgentFinalTurn,
    AgentFinalized,
    RootMailSuspended,
    AgentAsked {
        target: String,
        question: String,
        answer: Option<String>,
        error: Option<String>,
    },
    AgentStopped {
        target: String,
        reason: String,
        report: String,
    },
    AgentRequest {
        checkpoint: Box<crate::model::ChildRequest>,
    },
    Usage {
        usage: UsageRecord,
    },
    SettingChange {
        key: String,
        value: Value,
    },
    TitleChange {
        title: String,
        source: TitleSource,
    },
    HiddenForkOutcome {
        outcome: HiddenForkOutcome,
    },
    Compaction {
        summary: String,
    },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    #[default]
    Auto,
    User,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenForkOutcome {
    /// Text-answer contracts may complete without a supplied function call.
    #[serde(default)]
    pub accepted_plain_text: bool,
    pub purpose: String,
    pub copy: usize,
    pub steps: usize,
    pub succeeded: bool,
    pub reason: Option<String>,
    pub author_prefix_bytes: usize,
    pub author_prefix_sha256: String,
    pub cache_hit_observed: bool,
}
/// Durable execution metadata, separate from the existing registry row.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentExecution {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub spawning_context: Option<u32>,
    #[serde(default)]
    pub max_descendants: Option<u64>,
    #[serde(default)]
    pub model_policy: Vec<crate::ModelPolicy>,
    #[serde(default)]
    pub launch_id: String,
    #[serde(default)]
    pub original_task: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub agent_id: String,
    pub cwd: std::path::PathBuf,
    pub remaining_depth: Option<usize>,
    pub task: String,
    pub background: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub id: String,
    pub owner: String,
    pub kind: String,
    pub status: String,
    pub command: String,
    pub agent_id: Option<String>,
    pub started: String,
    pub updated: String,
    pub output_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Value>,
    pub report: Option<String>,
    pub transcript: Option<String>,
}
impl JobRecord {
    pub fn running(&self) -> bool {
        self.status == "running"
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    /// Total input, including cache reads and writes, on every wire.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    pub call_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub attempt: usize,
    #[serde(default)]
    pub input_estimate: Option<u64>,
    pub model: String,
    pub kind: String,
    pub tokens: Tokens,
    pub cost_usd: Option<f64>,
    pub partial: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Value>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Auth,
    Network,
    Billing,
    RateLimit,
    ContextOverflow,
    BadRequest,
    Unavailable,
    LockConflict,
    ReadOnly,
}
#[derive(Clone, Debug, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct EngineError {
    pub kind: ErrorKind,
    pub message: String,
    pub retryable: bool,
    pub retry_after_secs: Option<u64>,
}
impl EngineError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::BadRequest,
            message: message.into(),
            retryable: false,
            retry_after_secs: None,
        }
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Unavailable,
            message: message.into(),
            retryable: true,
            retry_after_secs: None,
        }
    }
    pub fn network(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Network,
            ..Self::unavailable(message)
        }
    }
    pub fn cancelled() -> Self {
        Self {
            kind: ErrorKind::Unavailable,
            message: "turn cancelled".into(),
            retryable: false,
            retry_after_secs: None,
        }
    }
}
/// Native wire items retain their original order, IDs, phases and opaque signatures.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeReplay {
    pub provider: String,
    pub model: String,
    pub wire: code_config::ProviderType,
    pub items: Vec<Value>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    #[default]
    EndTurn,
    ToolUse,
    OutputLimit,
    Refusal,
    Filtered,
    Paused,
    Incomplete,
    Cancelled,
}
impl CompletionStatus {
    pub fn executable(&self) -> bool {
        matches!(self, Self::EndTurn | Self::ToolUse)
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::ToolUse => "tool_use",
            Self::OutputLimit => "max_tokens",
            Self::Refusal => "refusal",
            Self::Filtered => "content_filter",
            Self::Paused => "pause_turn",
            Self::Incomplete => "incomplete",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub version: u32,
    #[serde(rename = "eventId")]
    pub id: String,
    pub session_id: String,
    pub agent_id: String,
    pub turn_id: Option<String>,
    pub attempt_id: Option<String>,
    pub sequence: u64,
    pub ts: String,
    #[serde(flatten)]
    pub event: HistoryEvent,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRecord {
    #[serde(default)]
    pub discussing: bool,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub questions: Vec<QuestionSpec>,
    #[serde(default)]
    pub answers: Option<Vec<QuestionAnswer>>,
    #[serde(default)]
    pub blocking: bool,
    pub id: String,
    pub asker: String,
    pub recipient: Option<String>,
    pub breadcrumb: String,
    pub text: String,
    pub answer: Option<String>,
    pub note: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QuestionKind {
    #[default]
    Text,
    Choice,
    Boolean,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionSpec {
    #[serde(default)]
    pub id: String,
    pub text: String,
    pub kind: QuestionKind,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default = "complete_call")]
    pub allow_other: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionAnswer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boolean: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Once,
    Session,
    Deny { feedback: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub id: String,
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub parent_call_id: Option<String>,
    pub agent_id: String,
    pub breadcrumb: String,
    pub tool: String,
    pub job_id: Option<String>,
    pub target: Option<String>,
    pub diff: Option<String>,
    pub command: Option<String>,
    pub requested: Value,
    pub resolved: Value,
    pub reason: String,
    #[serde(default)]
    pub sidecar_verdict: Option<String>,
    #[serde(default)]
    pub sidecar_reason: Option<String>,
    #[serde(default)]
    pub sidecar_hint: Option<String>,
    pub grant_key: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextMeter {
    pub used: u64,
    pub window: Option<u64>,
    pub output_reservation: u64,
    pub source: String,
    pub warning: bool,
}
/// The single reducer used by live commits, resume, snapshots and fork.
pub fn reduce(history: &mut Vec<HistoryEvent>, event: &HistoryEvent) {
    match event {
        HistoryEvent::SubscriptionNotice { text, .. } => {
            history.push(HistoryEvent::SystemReminder { text: text.clone() })
        }
        HistoryEvent::AgentRequest { .. } | HistoryEvent::CompactionDraftRejected { .. } => {}
        HistoryEvent::ThinkingUpdate {
            assistant,
            call_id,
            reasoning,
        } => {
            let target = if let Some(id) = call_id {
                history.iter_mut().find(|e|matches!(e,HistoryEvent::Assistant{tool_calls,..}if tool_calls.iter().any(|c|&c.id == id)))
            } else {
                history
                    .iter_mut()
                    .filter(|e| matches!(e, HistoryEvent::Assistant { .. }))
                    .nth(*assistant)
            };
            if let Some(HistoryEvent::Assistant {
                reasoning: prior,
                replay,
                ..
            }) = target
            {
                *prior = reasoning.clone();
                *replay = None;
            }
        }
        HistoryEvent::Pruned {
            history: replacement,
            ..
        } => *history = replacement.clone(),
        HistoryEvent::HistoryReplace {
            history: replacement,
            ..
        } => *history = replacement.clone(),
        HistoryEvent::CallUpdate { call, .. } => {
            for h in history.iter_mut().rev() {
                if let HistoryEvent::Assistant {
                    tool_calls, replay, ..
                } = h
                    && let Some(c) = tool_calls.iter_mut().find(|c| c.id == call.id)
                {
                    *c = call.clone();
                    if let Some(replay) = replay {
                        for item in &mut replay.items {
                            match replay.wire {
                                code_config::ProviderType::Anthropic
                                    if item["type"] == "tool_use" && item["id"] == call.id =>
                                {
                                    item["input"] = call.replay_arguments();
                                    item["name"] = Value::String(call.name.clone());
                                }
                                code_config::ProviderType::OpenaiResponses
                                    if item["type"] == "function_call"
                                        && (item["call_id"] == call.id
                                            || item["id"] == call.id) =>
                                {
                                    item["arguments"] =
                                        Value::String(call.replay_arguments().to_string());
                                    item["name"] = Value::String(call.name.clone());
                                }
                                _ => {}
                            }
                        }
                    }
                    break;
                }
            }
        }
        _ => history.push(event.clone()),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    /// Metadata only. Terminal bytes use Session::subscribe_terminals.
    TerminalChanged {
        terminal: crate::terminals::TerminalInfo,
    },
    DecisionUpdated {
        record: Box<crate::DecisionRecord>,
        open_count: usize,
    },
    GuidanceSelected {
        selection: crate::guidance_selector::GuidanceSelection,
    },
    SubscriptionNotice {
        agent_id: String,
        changes: Vec<crate::subscriptions::SubscriptionChange>,
        overflow: bool,
    },
    ScopeRequestUpdate {
        request: crate::scopes::ScopeRequest,
    },
    ReviewerGuidanceTrustRequest {
        request: crate::ReviewerGuidanceRequest,
    },
    IdleCompacted,
    RootCompactionSuggested {
        used: u64,
        window: u64,
        threshold: f64,
    },
    /// Background discovery completed; call Session::configuration for the new snapshot.
    DiscoveryUpdated {
        errors: Vec<String>,
    },
    GoalDraft {
        contract: crate::goal::GoalContract,
    },
    GoalUpdate {
        state: Box<crate::goal_controller::GoalRun>,
    },
    #[serde(rename_all = "camelCase")]
    ProposalRequest {
        agent_id: String,
        proposal: crate::goal::Proposal,
    },
    #[serde(rename_all = "camelCase")]
    RoadblockRequest {
        agent_id: String,
        roadblock: crate::goal::Roadblock,
    },
    /// The session's frontend queue changed; query Session::queued_messages.
    QueueChanged,
    QueuedMessageDelivered {
        message: crate::QueuedMessage,
    },
    MovedToBackground {
        call_id: String,
        job_id: String,
        reason: String,
    },
    #[serde(rename_all = "camelCase")]
    ForkChat {
        fork_id: String,
        question_id: String,
        asker: String,
        kind: String,
        text: String,
        messages: Vec<crate::ForkChatMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call: Option<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_result: Option<ToolResultData>,
    },
    TitleChanged {
        title: String,
        source: TitleSource,
    },
    HiddenForkFinished {
        outcome: HiddenForkOutcome,
    },
    #[serde(rename_all = "camelCase")]
    ScriptCall {
        call: ToolCall,
        parent_call_id: Option<String>,
        result: Option<ToolResultData>,
        job_id: Option<String>,
        agent_id: String,
        tool: String,
        args_summary: String,
        status: String,
    },
    /// Child display events are wrapped; canonical envelopes and usage already carry identity.
    #[serde(rename_all = "camelCase")]
    AgentEvent {
        agent_id: String,
        breadcrumb: String,
        event: Box<EngineEvent>,
    },
    AgentStarted {
        agent: crate::sessions::AgentRecord,
        execution: AgentExecution,
        breadcrumb: String,
        #[serde(default, rename = "taskCallId")]
        task_call_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    AgentStatus {
        agent_id: String,
        status: String,
        breadcrumb: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        question_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    AgentFinished {
        agent_id: String,
        status: String,
        report: String,
        transcript: String,
        breadcrumb: String,
    },
    AgentMessage {
        from: String,
        to: String,
        text: String,
        breadcrumb: String,
    },
    Job {
        job: JobRecord,
    },
    Committed {
        envelope: EventEnvelope,
    },
    Context {
        meter: ContextMeter,
    },
    ApprovalRequest {
        request: ApprovalRequest,
    },
    #[serde(rename_all = "camelCase")]
    SafetyReview {
        call_id: String,
        agent_id: String,
        breadcrumb: String,
        model: String,
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        verdict: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Question {
        question: QuestionRecord,
    },
    SessionClaimed {
        session_id: String,
        message: String,
    },
    PartialCompletion {
        status: CompletionStatus,
        native_reason: String,
        text: String,
    },
    Text {
        delta: String,
    },
    Reasoning {
        delta: String,
    },
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    ToolStart {
        call: ToolCall,
    },
    ToolEnd {
        id: String,
        name: String,
        output: String,
        is_error: bool,
        #[serde(flatten)]
        details: ToolResultDetails,
    },
    Todo {
        items: Value,
    },
    Usage {
        usage: UsageRecord,
    },
    Reconnecting {
        attempt: usize,
        delay_ms: u64,
    },
    Pruned {
        report: crate::PruneReport,
    },
    ModelSwitched {
        from: String,
        to: String,
        reason: String,
    },
    Retry {
        attempt: usize,
        delay_ms: u64,
        /// Why the attempt failed (the error's first line), for display.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Discard the current partial assistant response before replaying the request.
    StreamReset {
        attempt: usize,
    },
    Error {
        error: EngineError,
    },
    TurnEnd {
        stop_reason: String,
    },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTotals {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub known_cost_usd: f64,
    pub known_cost_calls: u64,
    pub unknown_cost_calls: u64,
}
impl SessionTotals {
    pub fn add(&mut self, usage: &UsageRecord) {
        self.calls += 1;
        self.input_tokens += usage.tokens.input_tokens.unwrap_or(0);
        self.output_tokens += usage.tokens.output_tokens.unwrap_or(0);
        self.cache_read_tokens += usage.tokens.cache_read_tokens.unwrap_or(0);
        self.cache_write_tokens += usage.tokens.cache_write_tokens.unwrap_or(0);
        match usage.cost_usd {
            Some(cost) => {
                self.known_cost_usd += cost;
                self.known_cost_calls += 1;
            }
            None => self.unknown_cost_calls += 1,
        }
    }
    pub fn cost_display(&self) -> String {
        if self.known_cost_calls == 0 {
            "cost unknown".into()
        } else {
            let precision = if self.known_cost_usd < 0.01 { 4 } else { 2 };
            format!(
                "${:.*}{}",
                precision,
                self.known_cost_usd,
                if self.unknown_cost_calls > 0 { "+" } else { "" }
            )
        }
    }
    pub fn summary(&self) -> String {
        format!(
            "Usage: {} calls; {} input, {} output, {} cache read, {} cache write tokens; {}",
            self.calls,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.cost_display()
        )
    }
}
