//! Explicit HTTP wires: request bytes, opaque reasoning and billing remain under our control.
pub mod command;
pub mod history;
pub(crate) mod sse;
use crate::{schema::*, think::ThinkSplitter, tools::ToolDefinition};
use code_config::{
    Billing, ModelConfig, PromptCaching, Provider, ProviderType, ReportedCost, ThinkingMode,
};
use futures_util::StreamExt;
use history::{anthropic_history, chat_history, responses_history};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    sync::mpsc::UnboundedSender,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type Result<T> = std::result::Result<T, EngineError>;
fn cache_hits() -> &'static Mutex<BTreeSet<String>> {
    static HITS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    HITS.get_or_init(Mutex::default)
}
pub type EventSender = UnboundedSender<EngineEvent>;
#[derive(Clone)]
pub struct Model {
    client: Client,
    provider: Provider,
    pub config: ModelConfig,
    pub provider_id: String,
    pub model_id: String,
    pub effort: Option<String>,
}
#[derive(Default)]
pub struct Completion {
    pub text: String,
    pub reasoning: Vec<ReasoningBlock>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: String,
    pub status: CompletionStatus,
    pub replay: Option<NativeReplay>,
    pub tokens: Tokens,
    pub call_id: String,
}
/// Stable bytes precede the volatile history. Later self-evaluation can compare these directly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedRequest {
    #[serde(default)]
    pub tool_aliases: BTreeMap<String, String>,
    #[serde(with = "utf8_bytes")]
    pub prefix: Vec<u8>,
    #[serde(with = "utf8_bytes")]
    pub body: Vec<u8>,
    wire: ProviderType,
    cache: Option<Value>,
    history_items: usize,
    pub offered_tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub capability_catalog: Option<Vec<ToolDefinition>>,
    pub session_id: String,
    /// Provider prompt-cache routing identity, independent of conversation identity.
    pub cache_key: String,
    /// Required command conversation/replay identity.
    #[serde(deserialize_with = "nonempty_identity")]
    pub conversation_id: String,
    pub agent_id: String,
    pub kind: String,
    pub input_estimate: u64,
    pub retry_limit: usize,
    #[serde(default)]
    pub fallback_ready: bool,
    #[serde(default)]
    pub command_context: command::RequestContext,
    #[serde(default)]
    pub assets_dir: Option<std::path::PathBuf>,
}
fn nonempty_identity<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let identity = String::deserialize(deserializer)?;
    if identity.trim().is_empty() {
        return Err(serde::de::Error::custom("conversation_id must be nonempty"));
    }
    Ok(identity)
}

const ERROR_BODY_LIMIT: usize = 64 * 1024;

/// Stop polling as soon as the bounded prefix is full, even for an endless body.
pub(crate) async fn error_body_prefix<B: AsRef<[u8]>, E>(
    stream: impl futures_util::Stream<Item = std::result::Result<B, E>>,
) -> std::result::Result<String, E> {
    futures_util::pin_mut!(stream);
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let chunk = chunk.as_ref();
        let take = chunk.len().min(ERROR_BODY_LIMIT - bytes.len());
        bytes.extend_from_slice(&chunk[..take]);
        if bytes.len() == ERROR_BODY_LIMIT {
            break;
        }
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    let mut end = text.len().min(ERROR_BODY_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    Ok(text)
}

mod utf8_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(std::str::from_utf8(bytes).map_err(serde::ser::Error::custom)?)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        Ok(String::deserialize(deserializer)?.into_bytes())
    }
}
/// Durable child request profile. Connection credentials are never included.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildRequest {
    #[serde(default)]
    pub provider_id: String,
    pub primary_config: ModelConfig,
    #[serde(default)]
    pub primary_effort: Option<String>,
    pub primary_caching: PromptCaching,
    pub request: PreparedRequest,
    pub system: String,
    pub compacted_estimate: Option<u64>,
    pub warned_context: bool,
    pub config: ModelConfig,
    pub effort: Option<String>,
    pub caching: PromptCaching,
    pub history_len: usize,
    pub history_hash: String,
    pub context: u32,
}
impl PreparedRequest {
    pub fn catalog(&self) -> &[ToolDefinition] {
        self.capability_catalog
            .as_deref()
            .unwrap_or(&self.offered_tools)
    }
    pub fn cache_identity(&self) -> &str {
        &self.cache_key
    }
    pub fn conversation_identity(&self) -> &str {
        &self.conversation_id
    }
    pub(crate) fn canonical_tool_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.tool_aliases
            .iter()
            .find_map(|(c, n)| (n == name).then_some(c.as_str()))
            .unwrap_or(name)
    }
    /// Reserve two Anthropic checkpoints for stable reviewer reference blocks.
    /// The system and automatic tail use the other two available checkpoints.
    pub(crate) fn cache_references(mut self) -> Result<Self> {
        if self.wire != ProviderType::Anthropic || self.cache.is_none() {
            return Ok(self);
        }
        let body: Value = serde_json::from_slice(&self.body)
            .map_err(|_| EngineError::bad("invalid reviewer request"))?;
        let mut messages = body["messages"].clone();
        let mut remaining = 2;
        for message in messages.as_array_mut().into_iter().flatten() {
            if message["role"] != "user" {
                continue;
            }
            for block in message["content"].as_array_mut().into_iter().flatten() {
                let stable = block["text"]
                    .as_str()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .is_some_and(|v| v.get("reference").is_some() || v.get("guidance").is_some());
                if stable && remaining > 0 {
                    block["cache_control"] = self.cache.clone().unwrap();
                    remaining -= 1;
                }
            }
        }
        self.body = self.prefix.clone();
        self.body.extend(
            serde_json::to_vec(&messages)
                .map_err(|_| EngineError::bad("reviewer serialization failed"))?,
        );
        self.body.push(b'}');
        self.input_estimate = count_request_tokens(&self.body);
        Ok(self)
    }
    /// Append a user message without rewriting the frozen author's bytes.
    pub fn with_user_tail(&self, text: &str) -> Result<Self> {
        if self.wire == ProviderType::Command && self.command_context.latest {
            return Err(EngineError::bad("history: latest disables hidden forks"));
        }
        self.with_history_tail(&[HistoryEvent::User {
            text: text.into(),
            attachments: vec![],
        }])
    }
    /// Hidden forks must preserve both the complete author history and request options.
    pub fn verify_fork_prefix(&self, branch: &Self) -> Result<()> {
        if self.body.len() < 2
            || branch.body.len() < 2
            || self.prefix != branch.prefix
            || self.cache_identity() != branch.cache_identity()
            || self.conversation_identity() != branch.conversation_identity()
            || self.capability_catalog != branch.capability_catalog
            || !self.shares_author_prefix(branch)
        {
            return Err(EngineError::bad(
                "hidden fork changed the author request prefix",
            ));
        }
        Ok(())
    }
    /// Preserve every byte of the previous input, including native replay and cache policy.
    pub(crate) fn with_history_tail(&self, history: &[HistoryEvent]) -> Result<Self> {
        let sanitized =
            crate::attachments::expand_tool_media(&crate::tool_names::sanitize_history(history));
        let history = sanitized.as_slice();
        let items = match self.wire {
            ProviderType::Anthropic => {
                anthropic_history(history, None, self.assets_dir.as_deref())?
            }
            ProviderType::OpenaiChat => {
                let mut items = chat_history("", history, self.assets_dir.as_deref())?;
                items.as_array_mut().unwrap().remove(0);
                items
            }
            ProviderType::Command => command::history(history),
            _ => responses_history(history, self.assets_dir.as_deref())?,
        };
        let items = items.as_array().unwrap();
        let mut next = self.clone();
        next.body.truncate(next.body.len() - 2);
        for (index, item) in items.iter().enumerate() {
            if self.history_items > 0 || index > 0 {
                next.body.push(b',');
            }
            next.body
                .extend(serde_json::to_vec(item).map_err(|e| EngineError::bad(e.to_string()))?);
        }
        next.body.extend_from_slice(b"]}");
        next.history_items += items.len();
        next.input_estimate = count_request_tokens(&next.body);
        Ok(next)
    }
    /// Disable tool choice for a final report while retaining tools[] byte-for-byte.
    /// Command providers are additionally enforced by the engine dispatcher.
    pub fn disable_tools(&mut self) -> Result<()> {
        if self.wire == ProviderType::Command || self.offered_tools.is_empty() {
            return Ok(());
        }
        let (old, new) = if self.wire == ProviderType::Anthropic {
            (json!({"type":"auto"}), json!({"type":"none"}))
        } else {
            (json!("auto"), json!("none"))
        };
        let old = format!("\"tool_choice\":{old}");
        let new = format!("\"tool_choice\":{new}");
        let prefix = std::str::from_utf8(&self.prefix)
            .map_err(|_| EngineError::bad("invalid request prefix"))?;
        if !prefix.contains(&old) {
            return Err(EngineError::bad("request tool choice is already forced"));
        }
        let prefix = prefix.replacen(&old, &new, 1).into_bytes();
        let mut body = prefix.clone();
        body.extend_from_slice(&self.body[self.prefix.len()..]);
        self.prefix = prefix;
        self.body = body;
        self.input_estimate = count_request_tokens(&self.body);
        Ok(())
    }
    pub(crate) fn enable_tools(&mut self) -> Result<()> {
        if self.wire == ProviderType::Command || self.offered_tools.is_empty() {
            return Ok(());
        }
        let (old, new) = if self.wire == ProviderType::Anthropic {
            (json!({"type":"none"}), json!({"type":"auto"}))
        } else {
            (json!("none"), json!("auto"))
        };
        let old = format!("\"tool_choice\":{old}");
        let new = format!("\"tool_choice\":{new}");
        let prefix = std::str::from_utf8(&self.prefix)
            .map_err(|_| EngineError::bad("invalid request prefix"))?;
        if !prefix.contains(&old) {
            return Ok(());
        }
        let prefix = prefix.replacen(&old, &new, 1).into_bytes();
        let mut body = prefix.clone();
        body.extend_from_slice(&self.body[self.prefix.len()..]);
        self.prefix = prefix;
        self.body = body;
        self.input_estimate = count_request_tokens(&self.body);
        Ok(())
    }
    /// Bytes through the end of the author's last history item, excluding array/object closers.
    /// A future self-evaluation request must start with these exact bytes.
    pub fn author_prefix(&self) -> &[u8] {
        &self.body[..self.body.len() - 2]
    }
    pub fn shares_author_prefix(&self, branch: &Self) -> bool {
        branch.body.starts_with(self.author_prefix())
    }
}
impl Model {
    fn endpoint_cache_key(&self) -> String {
        format!(
            "{:?}|{}|{}|{}",
            self.provider.kind,
            self.provider_id,
            self.provider.connection_url().unwrap_or(""),
            self.model_id
        )
    }
    /// Observed provider cache reads, shared across model instances for this endpoint/model.
    pub fn observed_cache_hit(&self) -> bool {
        cache_hits()
            .lock()
            .unwrap()
            .contains(&self.endpoint_cache_key())
    }
    pub fn new(
        provider_id: String,
        model_id: String,
        provider: Provider,
        config: ModelConfig,
        requested_effort: Option<&str>,
    ) -> Result<Self> {
        let provider = provider.for_model(&config);
        let effort = config
            .reasoning
            .resolve(requested_effort)
            .map_err(|e| EngineError::bad(e.to_string()))?;
        if !provider.enabled {
            return Err(EngineError::bad("provider is disabled"));
        }
        if !config
            .capabilities
            .output
            .contains(&code_config::Modality::Text)
        {
            return Err(EngineError::bad("chat requires a text-output model"));
        }
        if provider.kind != ProviderType::Command {
            let url = reqwest::Url::parse(provider.connection_url().unwrap_or(""))
                .map_err(|_| EngineError::bad("invalid provider baseUrl"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(EngineError::bad("provider baseUrl must be HTTP(S)"));
            }
        }
        if provider.kind == ProviderType::Command && provider.connection_command().is_none() {
            return Err(EngineError::bad("command provider requires command block"));
        }
        if config.max_output_tokens == Some(0) {
            return Err(EngineError::bad("maxOutputTokens must be positive"));
        }
        if provider.kind == ProviderType::Anthropic {
            match (config.thinking, effort.as_deref()) {
                (None, Some(_)) => {
                    return Err(EngineError::bad(
                        "Anthropic reasoning requires an explicit model thinking mode: adaptive, manual or none",
                    ));
                }
                (Some(ThinkingMode::Manual), Some(e)) => {
                    let budget = config
                        .reasoning
                        .budgets
                        .get(e)
                        .copied()
                        .map(Ok)
                        .unwrap_or_else(|| {
                            anthropic_budget(config.max_output_tokens.unwrap_or(8192), e)
                        })?;
                    if budget < 1024 || budget >= config.max_output_tokens.unwrap_or(8192) {
                        return Err(EngineError::bad(
                            "manual thinking budget must be >=1024 and < maxOutputTokens",
                        ));
                    }
                }
                (Some(ThinkingMode::Adaptive), Some(e))
                    if !["low", "medium", "high", "xhigh", "max"].contains(&e) =>
                {
                    return Err(EngineError::bad(
                        "unsupported adaptive thinking effort; configure reasoning.normalized",
                    ));
                }
                _ => {}
            }
        }
        let client = crate::http_client(|| {
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(30))
        })
        .map_err(|_| EngineError::bad("could not create HTTP client"))?;
        Ok(Self {
            client,
            provider,
            config,
            provider_id,
            model_id,
            effort,
        })
    }
    pub fn id(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
    /// The wire protocol requests are prepared for.
    pub fn wire(&self) -> ProviderType {
        self.provider.kind
    }
    pub fn caching(&self) -> PromptCaching {
        self.provider.prompt_caching.clone()
    }
    /// Concurrent-context caps configured for the provider and for this model.
    pub fn context_limits(&self) -> (Option<usize>, Option<usize>) {
        (
            self.provider.max_concurrent_contexts,
            self.config.max_concurrent_contexts,
        )
    }
    pub fn capabilities(&self) -> &code_config::Capabilities {
        &self.config.capabilities
    }
    pub fn context_window(&self) -> Option<u64> {
        self.config.context_window
    }
    /// Completion budget held back for output (`maxOutputTokens`, default 8192).
    pub fn output_reservation(&self) -> u64 {
        self.config.max_output_tokens.unwrap_or(8192)
    }
    /// Hold back exactly `tokens` for output on this model's requests.
    pub fn reserve_output(&mut self, tokens: u64) {
        self.config.max_output_tokens = Some(tokens);
    }
    /// `used` input tokens plus the output reservation reach the context window.
    pub fn budget_full(&self, used: u64) -> bool {
        self.context_window()
            .is_some_and(|w| used.saturating_add(self.output_reservation()) >= w)
    }
    pub fn prepare(
        &self,
        system: &str,
        tools: &[ToolDefinition],
        history: &[HistoryEvent],
        session_id: &str,
    ) -> Result<PreparedRequest> {
        self.prepare_with_assets(system, tools, history, session_id, None)
    }
    pub fn prepare_with_assets(
        &self,
        system: &str,
        tools: &[ToolDefinition],
        history: &[HistoryEvent],
        session_id: &str,
        assets_dir: Option<std::path::PathBuf>,
    ) -> Result<PreparedRequest> {
        crate::attachments::validate(&self.config.capabilities, history, assets_dir.as_deref())?;
        let mut history = crate::tool_names::sanitize_history(history);
        for event in &mut history {
            if let HistoryEvent::Assistant {
                replay, reasoning, ..
            } = event
                && replay.as_ref().is_some_and(|r| {
                    r.provider != self.provider_id
                        || r.model != self.model_id
                        || r.wire != self.provider.kind
                })
            {
                *replay = None;
                reasoning.clear();
            }
        }
        let history = crate::attachments::expand_tool_media(&history);
        let history = history.as_slice();
        let strict = self.provider.strict_tools(&self.config);
        let schema = |t: &ToolDefinition| {
            if strict {
                crate::wire_schema::for_responses(&t.schema)
            } else {
                crate::wire_schema::for_lenient(&t.schema)
            }
        };
        let max_tokens = self.config.max_output_tokens.unwrap_or(8192);
        let offered = self.config.capabilities.tools && !self.command_text();
        let definitions: Vec<Value> = if offered {
            tools.iter().map(|t| match self.provider.kind {
            ProviderType::Anthropic => json!({"name":t.name,"description":t.description,"input_schema":schema(t),"strict":strict}),
            ProviderType::OpenaiChat => json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":schema(t),"strict":strict}}),
            ProviderType::Command => json!({"name":t.name,"description":t.description,"schema":schema(t),"strict":strict}),
            _ => json!({"type":"function","name":t.name,"description":t.description,"parameters":schema(t),"strict":strict}),
        }).collect()
        } else {
            vec![]
        };
        let mut fields = vec![("model", json!(self.model_id)), ("stream", json!(true))];
        // OpenAI limits prompt_cache_key to 64 characters. Preserve the complete
        // logical identity in PreparedRequest/command routing; hash long wire keys.
        let wire_cache_key = if session_id.len() > 64 {
            use sha2::{Digest, Sha256};
            crate::hex::hex(&Sha256::digest(session_id.as_bytes()))
        } else {
            session_id.to_owned()
        };
        // Params and tools are serialized once in a fixed order before messages/input.
        let (key, messages) = match self.provider.kind {
            ProviderType::Command => {
                fields.push(("system", json!(system)));
                fields.push(("maxTokens", json!(max_tokens)));
                fields.push(("reasoningEffort", json!(self.effort)));
                fields.push(("tools", json!(definitions)));
                if offered {
                    fields.push(("tool_choice", json!("auto")));
                }
                ("messages", command::history(history))
            }
            ProviderType::Anthropic => {
                fields.push(("max_tokens", json!(max_tokens)));
                let cache = self.cache_control();
                if let Some(c) = &cache {
                    // Automatic caching advances the breakpoint without mutating prior blocks.
                    fields.push(("cache_control", c.clone()));
                }
                let mut block = json!({"type":"text","text":system});
                if let Some(c) = &cache {
                    block["cache_control"] = c.clone();
                }
                fields.push(("system", json!([block])));
                match self.config.thinking {
                    Some(ThinkingMode::Adaptive) => {
                        fields.push(("thinking", json!({"type":"adaptive"})));
                        if let Some(e) = &self.effort {
                            fields.push(("output_config", json!({"effort":e})));
                        }
                    }
                    Some(ThinkingMode::Manual) => {
                        let e = self.effort.as_deref().ok_or_else(|| {
                            EngineError::bad("manual thinking requires an effort")
                        })?;
                        let budget = self
                            .config
                            .reasoning
                            .budgets
                            .get(e)
                            .copied()
                            .map(Ok)
                            .unwrap_or_else(|| anthropic_budget(max_tokens, e))?;
                        fields.push(("thinking", json!({"type":"enabled","budget_tokens":budget})));
                    }
                    _ => {}
                }
                if offered {
                    fields.push(("tools", json!(definitions)));
                    fields.push(("tool_choice", json!({"type":"auto"})));
                }
                (
                    "messages",
                    anthropic_history(history, None, assets_dir.as_deref())?,
                )
            }
            ProviderType::OpenaiChat => {
                fields.push(("max_completion_tokens", json!(max_tokens)));
                fields.push(("stream_options", json!({"include_usage":true})));
                fields.push(("prompt_cache_key", json!(wire_cache_key)));
                if let Some(e) = &self.effort {
                    fields.push(("reasoning_effort", json!(e)));
                }
                if offered {
                    fields.push(("tools", json!(definitions)));
                    fields.push(("tool_choice", json!("auto")));
                }
                (
                    "messages",
                    chat_history(system, history, assets_dir.as_deref())?,
                )
            }
            _ => {
                fields.push(("max_output_tokens", json!(max_tokens)));
                fields.push(("instructions", json!(system)));
                fields.push(("store", json!(false)));
                fields.push(("include", json!(["reasoning.encrypted_content"])));
                fields.push(("prompt_cache_key", json!(wire_cache_key)));
                if let Some(e) = &self.effort {
                    fields.push(("reasoning", json!({"effort":e,"summary":"auto"})));
                }
                if offered {
                    fields.push(("tools", json!(definitions)));
                    fields.push(("tool_choice", json!("auto")));
                }
                ("input", responses_history(history, assets_dir.as_deref())?)
            }
        };
        let mut prefix = vec![b'{'];
        for (i, (name, value)) in fields.iter().enumerate() {
            if i != 0 {
                prefix.push(b',');
            }
            prefix.extend(
                serde_json::to_vec(name)
                    .map_err(|_| EngineError::bad("request serialization failed"))?,
            );
            prefix.push(b':');
            prefix.extend(
                serde_json::to_vec(value)
                    .map_err(|_| EngineError::bad("request serialization failed"))?,
            );
        }
        prefix.push(b',');
        prefix.extend(serde_json::to_vec(key).unwrap());
        prefix.push(b':');
        let mut body = prefix.clone();
        body.extend(
            serde_json::to_vec(&messages)
                .map_err(|_| EngineError::bad("history serialization failed"))?,
        );
        body.push(b'}');
        let input_estimate = count_request_tokens(&body);
        Ok(PreparedRequest {
            tool_aliases: BTreeMap::new(),
            input_estimate,
            retry_limit: crate::retry::MAX_RETRIES,
            fallback_ready: false,
            command_context: command::RequestContext::default(),
            assets_dir,
            prefix,
            body,
            wire: self.provider.kind,
            cache: if self.provider.kind == ProviderType::Anthropic {
                self.cache_control()
            } else {
                None
            },
            history_items: messages.as_array().map_or(0, Vec::len),
            offered_tools: if offered { tools.to_vec() } else { vec![] },
            capability_catalog: None,
            session_id: session_id.into(),
            cache_key: session_id.into(),
            conversation_id: session_id.into(),
            agent_id: "root".into(),
            kind: "agent".into(),
        })
    }
    pub fn command_latest(&self) -> bool {
        self.provider.latest_history_only(&self.config)
    }
    pub fn command_text(&self) -> bool {
        self.provider.text_command(&self.config)
    }
    fn cache_control(&self) -> Option<Value> {
        match self.provider.prompt_caching {
            PromptCaching::Off => None,
            PromptCaching::Auto => Some(json!({"type":"ephemeral"})),
            PromptCaching::OneHour => Some(json!({"type":"ephemeral","ttl":"1h"})),
        }
    }
    pub async fn stream(
        &self,
        request: &PreparedRequest,
        tx: &EventSender,
        cancel: &CancellationToken,
    ) -> Result<Completion> {
        // Adapted from FlyCockpit's Cockpit: restart build+drain, never resume partial JSON/tool args.
        let retry_limit = request.retry_limit.min(crate::retry::MAX_RETRIES);
        let mut attempt = 0usize;
        loop {
            crate::exclusive::check_request(&request.session_id, &request.kind, &self.id())?;
            let call_id = Uuid::new_v4().to_string();
            let mut state = State {
                strip_leaks: self.config.text_tool_recovery,
                ..State::default()
            };
            let result = self.attempt(request, tx, cancel, &mut state).await;
            let mut usage = self.usage(call_id.clone(), &state, result.is_err());
            usage.session_id = request.session_id.clone();
            usage.agent_id = request.agent_id.clone();
            usage.kind = request.kind.clone();
            usage.attempt = attempt + 1;
            usage.input_estimate = (!self.command_latest()).then_some(request.input_estimate);
            if usage.tokens.cache_read_tokens.unwrap_or(0) > 0 {
                cache_hits()
                    .lock()
                    .unwrap()
                    .insert(self.endpoint_cache_key());
            }
            let _ = tx.send(EngineEvent::Usage { usage });
            match result {
                Ok(()) => {
                    let mut completion =
                        state.finish(tx, self.provider.kind, &self.provider_id, &self.model_id)?;
                    completion.call_id = call_id;
                    return Ok(completion);
                }
                Err(_) if self.provider.kind == ProviderType::Command && cancel.is_cancelled() => {
                    state.calls.clear();
                    state.stop_reason = "cancelled".into();
                    let mut completion =
                        state.finish(tx, self.provider.kind, &self.provider_id, &self.model_id)?;
                    completion.call_id = call_id;
                    return Ok(completion);
                }
                Err(e)
                    if e.retryable
                        && !cancel.is_cancelled()
                        && (attempt
                            < if request.fallback_ready
                                && matches!(e.kind, ErrorKind::Network | ErrorKind::Unavailable)
                            {
                                retry_limit.min(1)
                            } else {
                                retry_limit
                            }
                            || (e.kind == ErrorKind::Network
                                && !request.fallback_ready
                                && retry_limit > 0)) =>
                {
                    let delay = e
                        .retry_after_secs
                        .map(|n| Duration::from_secs(n.min(300)))
                        .unwrap_or(crate::retry::backoff(attempt));
                    let _ = tx.send(EngineEvent::Retry {
                        attempt: attempt + 1,
                        delay_ms: delay.as_millis() as u64,
                        reason: e.message.lines().next().map(str::to_owned),
                    });
                    if e.kind == ErrorKind::Network {
                        let _ = tx.send(EngineEvent::Reconnecting {
                            attempt: attempt + 1,
                            delay_ms: delay.as_millis() as u64,
                        });
                        let base = self.provider.connection_url().unwrap_or_default();
                        tokio::select! {
                            _ = cancel.cancelled() => return Err(EngineError::cancelled()),
                            _ = tokio::time::sleep(delay) => {},
                            _ = crate::retry::reconnected(base) => {},
                        }
                    } else {
                        tokio::select! { _ = cancel.cancelled() => return Err(EngineError::cancelled()), _ = tokio::time::sleep(delay) => {} }
                    }
                    let _ = tx.send(EngineEvent::StreamReset {
                        attempt: attempt + 1,
                    });
                }
                Err(e) => return Err(e),
            }
            attempt = attempt.saturating_add(1);
        }
    }
    fn usage(&self, call_id: String, state: &State, partial: bool) -> UsageRecord {
        let tokens = state.tokens(self.provider.kind);
        let reported = state.reported_cost.or_else(|| {
            (self.provider.kind == ProviderType::Command)
                .then(|| state.usage["costUsd"].as_f64())
                .flatten()
                .and_then(|v| code_config::CostUnit::Usd.dollars(v))
        });
        let cost = reported.or_else(|| {
            if partial {
                return None;
            }
            if matches!(self.config.billing, Billing::Subscription) && self.config.cost.is_none() {
                return Some(0.0);
            }
            let rates = self.config.cost.as_ref()?;
            let input = tokens.input_tokens?;
            let output = tokens.output_tokens?;
            let reads = tokens.cache_read_tokens.unwrap_or(0);
            let writes = tokens.cache_write_tokens.unwrap_or(0);
            let charge = |n, rate: Option<f64>| {
                if n == 0 {
                    Some(0.0)
                } else {
                    rate.map(|r| n as f64 * r / 1_000_000.0)
                }
            };
            Some(
                charge(input.saturating_sub(reads + writes), rates.input_per_m_tok)?
                    + charge(output, rates.output_per_m_tok)?
                    + charge(reads, rates.cache_read_per_m_tok)?
                    + charge(writes, rates.cache_write_per_m_tok)?,
            )
        });
        UsageRecord {
            call_id,
            session_id: String::new(),
            agent_id: String::new(),
            provider: self.provider_id.clone(),
            endpoint: format!("{}:{:?}", self.provider_id, self.provider.kind),
            attempt: 0,
            input_estimate: None,
            model: self.id(),
            kind: "agent".into(),
            tokens,
            cost_usd: cost,
            partial,
            context_used: state.usage["contextUsed"].as_u64(),
            context_window: state.usage["contextWindow"].as_u64(),
            quota: state.quota.clone(),
        }
    }
    async fn attempt(
        &self,
        request: &PreparedRequest,
        tx: &EventSender,
        cancel: &CancellationToken,
        state: &mut State,
    ) -> Result<()> {
        if self.provider.kind == ProviderType::Command {
            let target = command::Target::new(
                &self.provider_id,
                &self.model_id,
                &self.provider,
                &self.config,
            );
            return Box::pin(command::attempt(&target, request, tx, cancel, state)).await;
        }
        let base = self
            .provider
            .connection_url()
            .ok_or_else(|| EngineError::bad("missing baseUrl"))?
            .trim_end_matches('/');
        let suffix = match self.provider.kind {
            ProviderType::Anthropic => "/v1/messages",
            ProviderType::OpenaiChat => "/chat/completions",
            _ => "/responses",
        };
        let mut headers = HeaderMap::new();
        for (k, v) in self.provider.connection_headers() {
            headers.insert(
                HeaderName::from_bytes(k.as_bytes())
                    .map_err(|_| EngineError::bad("invalid provider header name"))?,
                HeaderValue::from_str(v)
                    .map_err(|_| EngineError::bad("invalid provider header value"))?,
            );
        }
        if self.provider.kind == ProviderType::Anthropic
            && !headers.contains_key("anthropic-version")
        {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        let start = Instant::now();
        let first_deadline = start + Duration::from_secs(self.provider.timeouts.first_token_secs);
        let request = self
            .client
            .post(format!("{base}{suffix}"))
            .headers(headers)
            .header("content-type", "application/json")
            .body(request.body.clone())
            .send();
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(EngineError::cancelled()),
            response = timeout_at(first_deadline,request) => response.map_err(|_|EngineError::network("first-token timeout"))?.map_err(|_|EngineError::network("provider network error"))?,
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let after = response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(crate::retry::retry_after);
            let body = tokio::select! { _ = cancel.cancelled() => return Err(EngineError::cancelled()), b = timeout_at(first_deadline,error_body_prefix(response.bytes_stream())) => b.ok().and_then(|v|v.ok()).unwrap_or_default() };
            let mut err = classify(status, &body);
            err.retry_after_secs = after;
            return Err(err);
        }
        let locator = self.provider.reported_cost(&self.config);
        if let Some(ReportedCost::Header { name, unit }) = locator {
            state.reported_cost = response
                .headers()
                .get(name)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<f64>().ok())
                .and_then(|v| unit.dollars(v));
        }
        let is_json = response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|s| s.contains("application/json"));
        let mut bytes = response.bytes_stream();
        if is_json {
            let mut body = Vec::new();
            loop {
                let chunk = tokio::select! { _ = cancel.cancelled() => return Err(EngineError::cancelled()), c = timeout_at(first_deadline,bytes.next()) => c.map_err(|_| EngineError::network("response timeout"))? };
                let Some(chunk) = chunk else {
                    break;
                };
                body.extend_from_slice(
                    &chunk.map_err(|_| EngineError::network("response interrupted"))?,
                );
                if body.len() > 32 * 1024 * 1024 {
                    return Err(EngineError::bad("response exceeds 32 MiB"));
                }
            }
            let value: Value = serde_json::from_slice(&body)
                .map_err(|_| EngineError::bad("malformed provider JSON"))?;
            if is_provider_error(&value) {
                return Err(classify(500, &value.to_string()));
            }
            state.observe_cost(locator, &value);
            state.body(self.provider.kind, &value, tx)?;
            return Ok(());
        }
        let mut parser = sse::Parser::default();
        let mut last_event = Instant::now();
        loop {
            let deadline = if state.semantic {
                last_event + Duration::from_secs(self.provider.timeouts.idle_secs)
            } else {
                first_deadline
            };
            let next = tokio::select! { _ = cancel.cancelled() => return Err(EngineError::cancelled()), next = timeout_at(deadline,bytes.next()) => next.map_err(|_|EngineError::network(if state.semantic {"idle timeout"} else {"first-token timeout"}))? };
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|_| EngineError::network("provider stream interrupted"))?;
            for data in parser.push(&chunk)? {
                if data.trim().is_empty() {
                    continue;
                }
                if data.trim() == "[DONE]" {
                    state.done = true;
                    break;
                }
                let value: Value = serde_json::from_str(&data)
                    .map_err(|_| EngineError::unavailable("malformed provider SSE data"))?;
                if is_provider_error(&value) {
                    let error = value.get("error").unwrap_or(&value);
                    let status = error["code"]
                        .as_u64()
                        .and_then(|c| u16::try_from(c).ok())
                        .unwrap_or(500);
                    return Err(classify(status, &error.to_string()));
                }
                state.observe_cost(locator, &value);
                if matches!(
                    value["type"].as_str(),
                    Some("response.completed" | "response.incomplete")
                ) {
                    state.observe_cost(locator, &value["response"]);
                }
                let activity = state.activity;
                state.event(self.provider.kind, &value, tx)?;
                if state.activity != activity {
                    last_event = Instant::now();
                }
                if state.done {
                    break;
                }
            }
            // Terminal protocol boundaries own completion; EOF/idle after them is irrelevant.
            if state.done {
                break;
            }
        }
        if !state.done {
            return Err(EngineError::network(
                "provider stream ended before completion",
            ));
        }
        Ok(())
    }
}
fn is_provider_error(value: &Value) -> bool {
    value.get("error").is_some_and(|error| !error.is_null()) || value["type"] == "error"
}

fn classify(status: u16, body: &str) -> EngineError {
    let lower = body.to_ascii_lowercase();
    let usage_limited = status == 402
        || [
            "insufficient_quota",
            "insufficient credits",
            "credit balance",
            "usage_limit",
            "usage limit",
            "quota exceeded",
        ]
        .iter()
        .any(|s| lower.contains(s));
    let kind = if lower.contains("authentication_error") || lower.contains("invalid_api_key") {
        ErrorKind::Auth
    } else if usage_limited {
        ErrorKind::Billing
    } else if lower.contains("overloaded") || lower.contains("overload_error") {
        ErrorKind::Unavailable
    } else if lower.contains("rate_limit_error") {
        ErrorKind::RateLimit
    } else if lower.contains("invalid_request_error") && !lower.contains("context") {
        ErrorKind::BadRequest
    } else if lower.contains("context_length")
        || lower.contains("context window")
        || lower.contains("too many tokens")
        || lower.contains("prompt is too long")
    {
        ErrorKind::ContextOverflow
    } else {
        match status {
            401 | 403 => ErrorKind::Auth,
            429 => ErrorKind::RateLimit,
            408 | 500..=599 => ErrorKind::Unavailable,
            _ => ErrorKind::BadRequest,
        }
    };
    EngineError {
        kind,
        message: format!(
            "provider HTTP {status}: {}",
            match kind {
                ErrorKind::Auth => "authentication failed",
                ErrorKind::Billing => "billing or usage limit reached",
                ErrorKind::Network => "network unavailable",
                ErrorKind::RateLimit => "rate limited",
                ErrorKind::ContextOverflow =>
                    "context limit reached; compact or start a new session",
                ErrorKind::BadRequest => "request rejected (check provider/model configuration)",
                ErrorKind::Unavailable => "service unavailable",
                ErrorKind::LockConflict | ErrorKind::ReadOnly => "session unavailable",
            }
        ),
        retryable: !usage_limited && matches!(kind, ErrorKind::RateLimit | ErrorKind::Unavailable),
        retry_after_secs: None,
    }
}
#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    args: String,
    complete: bool,
}
#[derive(Default)]
struct State {
    text: String,
    reasoning: BTreeMap<usize, ReasoningBlock>,
    calls: BTreeMap<usize, PendingCall>,
    usage: Value,
    quota: Option<Value>,
    reported_cost: Option<f64>,
    done: bool,
    semantic: bool,
    stop_reason: String,
    splitter: ThinkSplitter,
    activity: u64,
    response_indices: BTreeMap<String, usize>,
    native: BTreeMap<usize, Value>,
    refusal: bool,
    strip_leaks: bool,
    leaks: crate::text_recovery::LeakStripper,
    /// Memory retained by command-provider reasoning `details`.
    details_cost: u64,
}
impl State {
    fn text(&mut self, s: &str, tx: &EventSender) {
        if s.is_empty() {
            return;
        }
        self.semantic = true;
        self.activity += 1;
        let cleaned = if self.strip_leaks {
            self.leaks.push(s, false)
        } else {
            s.to_owned()
        };
        let (visible, thinking) = self.splitter.push(&cleaned);
        self.text.push_str(&visible);
        self.reason(usize::MAX, &thinking, tx);
        if !visible.is_empty() {
            let _ = tx.send(EngineEvent::Text { delta: visible });
        }
    }
    fn reason(&mut self, index: usize, s: &str, tx: &EventSender) {
        if s.is_empty() {
            return;
        }
        self.semantic = true;
        self.activity += 1;
        self.reasoning.entry(index).or_default().text.push_str(s);
        let _ = tx.send(EngineEvent::Reasoning { delta: s.into() });
    }
    fn call(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args: &str,
        tx: &EventSender,
    ) {
        self.semantic = true;
        self.activity += 1;
        let call = self.calls.entry(index).or_default();
        if let Some(id) = id
            && !id.is_empty()
        {
            call.id = id.into();
        }
        if let Some(name) = name {
            call.name.push_str(name);
        }
        call.args.push_str(args);
        let _ = tx.send(EngineEvent::ToolCallDelta {
            index,
            id: id.map(str::to_owned),
            name: name.map(str::to_owned),
            arguments: args.into(),
        });
    }
    fn usage(&mut self, v: &Value) {
        if !v.is_object() {
            return;
        }
        if !self.usage.is_object() {
            self.usage = json!({});
        }
        for (key, value) in v.as_object().unwrap() {
            if !value.is_null() {
                self.usage[key] = value.clone();
            }
        }
    }
    fn progress(&mut self) {
        self.semantic = true;
        self.activity += 1;
    }
    fn observe_cost(&mut self, locator: Option<&ReportedCost>, value: &Value) {
        if let Some(ReportedCost::Body { pointer, unit }) = locator
            && let Some(value) = value
                .pointer(pointer)
                .and_then(Value::as_f64)
                .and_then(|v| unit.dollars(v))
        {
            self.reported_cost = Some(value);
        }
    }
    fn body(&mut self, kind: ProviderType, v: &Value, tx: &EventSender) -> Result<()> {
        match kind {
            ProviderType::Anthropic => {
                self.usage(&v["usage"]);
                if let Some(blocks) = v["content"].as_array() {
                    for (index, b) in blocks.iter().enumerate() {
                        self.event(
                            kind,
                            &json!({"type":"content_block_start","index":index,"content_block":b}),
                            tx,
                        )?;
                        self.event(
                            kind,
                            &json!({"type":"content_block_stop","index":index}),
                            tx,
                        )?;
                    }
                }
                self.stop_reason = v["stop_reason"].as_str().unwrap_or("").into();
                self.done = true;
            }
            ProviderType::OpenaiChat => {
                let mut message = v["choices"][0]["message"].clone();
                if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                    for (index, call) in calls.iter_mut().enumerate() {
                        if let Some(call) = call.as_object_mut() {
                            call.entry("index").or_insert_with(|| json!(index));
                        }
                    }
                }
                self.event(kind, &json!({"usage":v["usage"],"choices":[{"delta":message,"finish_reason":v["choices"][0]["finish_reason"]}]}),tx)?;
                self.done = true;
            }
            _ => {
                if v["status"] == "failed" {
                    return Err(classify(500, &v["error"].to_string()));
                }
                if !matches!(v["status"].as_str(), Some("completed" | "incomplete")) {
                    return Err(EngineError::bad("Responses body has no terminal status"));
                }
                self.event(kind, &json!({"type": if v["status"] == "incomplete" { "response.incomplete" } else { "response.completed" },"response":v}),tx)?;
            }
        }
        Ok(())
    }
    fn event(&mut self, kind: ProviderType, v: &Value, tx: &EventSender) -> Result<()> {
        match kind {
            ProviderType::OpenaiChat => {
                self.usage(&v["usage"]);
                let choice = &v["choices"][0];
                let d = &choice["delta"];
                if let Some(s) = d["content"].as_str() {
                    self.text(s, tx);
                }
                if let Some(s) = d["refusal"].as_str() {
                    self.refusal = true;
                    self.text(s, tx);
                }
                if let Some(s) = d["reasoning"]
                    .as_str()
                    .or_else(|| d["reasoning_content"].as_str())
                {
                    self.reason(0, s, tx);
                    self.reasoning.entry(0).or_default().field = Some(
                        if d["reasoning"].is_string() {
                            "reasoning"
                        } else {
                            "reasoning_content"
                        }
                        .into(),
                    );
                }
                if let Some(details) = d["reasoning_details"].as_array() {
                    for detail in details {
                        self.progress();
                        let index = detail["index"]
                            .as_u64()
                            .map(|n| n as usize)
                            .or_else(|| {
                                let id = detail["id"].as_str()?;
                                self.reasoning
                                    .iter()
                                    .find(|(_, r)| r.details.iter().any(|d| d["id"] == id))
                                    .map(|(i, _)| *i)
                            })
                            .unwrap_or_else(|| {
                                self.reasoning.keys().next_back().copied().unwrap_or(0)
                            });
                        if d["reasoning"]
                            .as_str()
                            .or_else(|| d["reasoning_content"].as_str())
                            .is_none()
                            && let Some(s) = detail["text"]
                                .as_str()
                                .or_else(|| detail["summary"].as_str())
                        {
                            self.reason(index, s, tx);
                        }
                        merge_detail(
                            &mut self.reasoning.entry(index).or_default().details,
                            detail,
                        );
                    }
                }
                if let Some(calls) = d["tool_calls"].as_array() {
                    for c in calls {
                        self.call(
                            c["index"].as_u64().unwrap_or(0) as usize,
                            c["id"].as_str(),
                            c["function"]["name"].as_str(),
                            c["function"]["arguments"].as_str().unwrap_or(""),
                            tx,
                        );
                    }
                }
                if let Some(reason) = choice["finish_reason"].as_str() {
                    self.stop_reason = reason.into();
                    if matches!(reason, "tool_calls" | "stop") {
                        for c in self.calls.values_mut() {
                            c.complete = true;
                        }
                    }
                }
            }
            ProviderType::Anthropic => {
                let index = v["index"].as_u64().unwrap_or(0) as usize;
                match v["type"].as_str().unwrap_or("") {
                    "message_start" => self.usage(&v["message"]["usage"]),
                    "content_block_start" => {
                        let b = &v["content_block"];
                        self.native.insert(index, b.clone());
                        self.progress();
                        match b["type"].as_str().unwrap_or("") {
                            "tool_use" => {
                                self.call(index, b["id"].as_str(), b["name"].as_str(), "", tx);
                                if b["input"].as_object().is_some_and(|o| !o.is_empty()) {
                                    self.calls.get_mut(&index).unwrap().args =
                                        b["input"].to_string();
                                }
                            }
                            "thinking" => {
                                self.reason(index, b["thinking"].as_str().unwrap_or(""), tx)
                            }
                            "redacted_thinking" => self
                                .reasoning
                                .entry(index)
                                .or_default()
                                .details
                                .push(b.clone()),
                            "text" => self.text(b["text"].as_str().unwrap_or(""), tx),
                            _ => {}
                        }
                    }
                    "content_block_delta" => {
                        let d = &v["delta"];
                        if let Some(b) = self.native.get_mut(&index) {
                            if d["type"] == "citations_delta" {
                                if !b["citations"].is_array() {
                                    b["citations"] = json!([]);
                                }
                                b["citations"]
                                    .as_array_mut()
                                    .unwrap()
                                    .push(d["citation"].clone());
                            }
                            let pair = match d["type"].as_str().unwrap_or("") {
                                "text_delta" => Some(("text", "text")),
                                "thinking_delta" => Some(("thinking", "thinking")),
                                "signature_delta" => Some(("signature", "signature")),
                                _ => None,
                            };
                            if let Some((to, from)) = pair {
                                let mut text = b[to].as_str().unwrap_or("").to_owned();
                                text.push_str(d[from].as_str().unwrap_or(""));
                                b[to] = json!(text);
                            }
                        }
                        if d["signature"].as_str().is_some_and(|s| !s.is_empty()) {
                            self.progress();
                        }
                        match d["type"].as_str().unwrap_or("") {
                            "text_delta" => self.text(d["text"].as_str().unwrap_or(""), tx),
                            "thinking_delta" => {
                                self.reason(index, d["thinking"].as_str().unwrap_or(""), tx)
                            }
                            "signature_delta" => self
                                .reasoning
                                .entry(index)
                                .or_default()
                                .signature
                                .get_or_insert_default()
                                .push_str(d["signature"].as_str().unwrap_or("")),
                            "input_json_delta" => self.call(
                                index,
                                None,
                                None,
                                d["partial_json"].as_str().unwrap_or(""),
                                tx,
                            ),
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        if let Some(c) = self.calls.get_mut(&index) {
                            c.complete = true;
                            if let Some(b) = self.native.get_mut(&index) {
                                b["input"] =
                                    serde_json::from_str(&c.args).unwrap_or_else(|_| json!({}));
                            }
                        }
                    }
                    "message_delta" => {
                        self.usage(&v["usage"]);
                        self.stop_reason = v["delta"]["stop_reason"]
                            .as_str()
                            .unwrap_or("end_turn")
                            .into();
                    }
                    "message_stop" => self.done = true,
                    _ => {}
                }
            }
            _ => {
                let index = v["output_index"].as_u64().unwrap_or(0) as usize;
                match v["type"].as_str().unwrap_or("") {
                    "response.output_text.delta" => {
                        self.text(v["delta"].as_str().unwrap_or(""), tx)
                    }
                    "response.reasoning_summary_text.delta"
                    | "response.reasoning_text.delta"
                    | "response.reasoning.delta" => {
                        if !v["delta"].is_null() && v["delta"] != "" {
                            self.progress();
                        }
                        self.reason(index, v["delta"].as_str().unwrap_or(""), tx)
                    }
                    "response.refusal.delta" => {
                        self.refusal = true;
                        self.text(v["delta"].as_str().unwrap_or(""), tx);
                    }
                    "response.output_item.added" => {
                        let item = &v["item"];
                        self.native.insert(index, item.clone());
                        self.progress();
                        if let Some(id) = item["id"].as_str() {
                            self.response_indices.insert(id.into(), index);
                        }
                        if item["type"] == "function_call" {
                            self.call(
                                index,
                                item["call_id"].as_str().or_else(|| item["id"].as_str()),
                                item["name"].as_str(),
                                item["arguments"].as_str().unwrap_or(""),
                                tx,
                            );
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        let i = v["item_id"]
                            .as_str()
                            .and_then(|id| self.response_indices.get(id))
                            .copied()
                            .unwrap_or(index);
                        self.call(i, None, None, v["delta"].as_str().unwrap_or(""), tx);
                    }
                    "response.output_item.done" => {
                        let item = &v["item"];
                        self.native.insert(index, item.clone());
                        self.progress();
                        if item["type"] == "reasoning" {
                            self.reasoning.entry(index).or_default().details = vec![item.clone()];
                        }
                        if item["type"] == "function_call" {
                            if !self.calls.contains_key(&index) {
                                self.call(
                                    index,
                                    item["call_id"].as_str(),
                                    item["name"].as_str(),
                                    "",
                                    tx,
                                );
                            }
                            if let Some(args) = item["arguments"].as_str() {
                                self.calls.get_mut(&index).unwrap().args = args.into();
                            }
                            self.calls.get_mut(&index).unwrap().complete =
                                item["status"] != "incomplete";
                        }
                    }
                    "response.completed" | "response.incomplete" => {
                        self.usage(&v["response"]["usage"]);
                        self.done = true;
                        if let Some(items) = v["response"]["output"].as_array() {
                            self.native.clear();
                            self.calls.clear();
                            if self.text.is_empty() {
                                let text = items
                                    .iter()
                                    .filter_map(|item| item["content"].as_array())
                                    .flatten()
                                    .filter_map(|block| {
                                        block["text"].as_str().or_else(|| block["refusal"].as_str())
                                    })
                                    .collect::<String>();
                                self.text(&text, tx);
                            }
                            for (index, item) in items.iter().enumerate() {
                                self.native.insert(index, item.clone());
                                if item["type"] == "function_call" {
                                    let c = self.calls.entry(index).or_default();
                                    c.id = item["call_id"]
                                        .as_str()
                                        .or_else(|| item["id"].as_str())
                                        .unwrap_or("")
                                        .into();
                                    c.name = item["name"].as_str().unwrap_or("").into();
                                    c.args = item["arguments"].as_str().unwrap_or("").into();
                                    c.complete = item["status"] != "incomplete";
                                }
                                if item["type"] == "message"
                                    && let Some(content) = item["content"].as_array()
                                {
                                    for block in content {
                                        if block["type"] == "refusal" {
                                            self.refusal = true;
                                            if self.text.is_empty() {
                                                self.text(
                                                    block["refusal"].as_str().unwrap_or(""),
                                                    tx,
                                                );
                                            }
                                        }
                                        if block["type"] == "output_text" && self.text.is_empty() {
                                            self.text(block["text"].as_str().unwrap_or(""), tx);
                                        }
                                    }
                                }
                            }
                        }
                        self.stop_reason = if v["type"] == "response.incomplete" {
                            v["response"]["incomplete_details"]["reason"]
                                .as_str()
                                .unwrap_or("incomplete")
                        } else if self.calls.is_empty() {
                            "end_turn"
                        } else {
                            "tool_use"
                        }
                        .into();
                    }
                    "response.failed" => {
                        return Err(classify(500, &v["response"]["error"].to_string()));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
    fn tokens(&self, kind: ProviderType) -> Tokens {
        let v = &self.usage;
        if kind == ProviderType::Command {
            return Tokens {
                input_tokens: v["inputTokens"].as_u64(),
                output_tokens: v["outputTokens"].as_u64(),
                cache_read_tokens: v["cacheReadTokens"].as_u64(),
                cache_write_tokens: v["cacheWriteTokens"].as_u64(),
                reasoning_tokens: v["reasoningTokens"].as_u64(),
            };
        }
        let read = v["cache_read_input_tokens"]
            .as_u64()
            .or_else(|| v["prompt_tokens_details"]["cached_tokens"].as_u64())
            .or_else(|| v["input_tokens_details"]["cached_tokens"].as_u64());
        let write = v["cache_creation_input_tokens"]
            .as_u64()
            .or_else(|| v["prompt_tokens_details"]["cache_write_tokens"].as_u64());
        let input = v["input_tokens"]
            .as_u64()
            .or_else(|| v["prompt_tokens"].as_u64());
        Tokens {
            input_tokens: input.map(|n| {
                if kind == ProviderType::Anthropic {
                    n + read.unwrap_or(0) + write.unwrap_or(0)
                } else {
                    n
                }
            }),
            output_tokens: v["output_tokens"]
                .as_u64()
                .or_else(|| v["completion_tokens"].as_u64()),
            cache_read_tokens: read,
            cache_write_tokens: write,
            reasoning_tokens: v["output_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .or_else(|| v["output_tokens_details"]["thinking_tokens"].as_u64())
                .or_else(|| v["completion_tokens_details"]["reasoning_tokens"].as_u64()),
        }
    }
    fn finish(
        mut self,
        tx: &EventSender,
        wire: ProviderType,
        provider: &str,
        model: &str,
    ) -> Result<Completion> {
        let tokens = self.tokens(wire);
        if self.strip_leaks {
            let pending = self.leaks.push("", true);
            let (text, thinking) = self.splitter.push(&pending);
            self.text.push_str(&text);
            self.reason(usize::MAX, &thinking, tx);
            if !text.is_empty() {
                let _ = tx.send(EngineEvent::Text { delta: text });
            }
        }
        let (text, thinking) = self.splitter.finish();
        self.text.push_str(&text);
        self.reason(usize::MAX, &thinking, tx);
        if !text.is_empty() {
            let _ = tx.send(EngineEvent::Text { delta: text });
        }
        let mut tool_calls = vec![];
        for (index, c) in self.calls {
            let arguments = if c.args.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.args).unwrap_or(Value::String(c.args))
            };
            let complete = c.complete;
            tool_calls.push(ToolCall {
                complete,
                id: if c.id.is_empty() {
                    format!("call_{}_{index}", Uuid::new_v4().simple())
                } else {
                    c.id
                },
                name: c.name,
                arguments,
            });
        }
        // Associate a signature-only close with plaintext accumulated on the parallel channel.
        for r in self.reasoning.values_mut() {
            if r.details.len() == 1
                && r.details[0]["type"] == "reasoning.text"
                && r.details[0]["text"].as_str().is_none_or(str::is_empty)
                && !r.text.is_empty()
            {
                r.details[0]["text"] = json!(r.text);
            }
        }
        let status = if self.refusal {
            CompletionStatus::Refusal
        } else {
            match self.stop_reason.as_str() {
                "length" | "max_tokens" | "max_output_tokens" => CompletionStatus::OutputLimit,
                "refusal" => CompletionStatus::Refusal,
                "content_filter" | "content_filtered" => CompletionStatus::Filtered,
                "pause_turn" => CompletionStatus::Paused,
                "tool_calls" | "tool_use" => CompletionStatus::ToolUse,
                "stop" | "end_turn" => CompletionStatus::EndTurn,
                "cancelled" => CompletionStatus::Cancelled,
                _ => CompletionStatus::Incomplete,
            }
        };
        let replay = if matches!(
            wire,
            ProviderType::Anthropic | ProviderType::OpenaiResponses
        ) && !self.leaks.stripped
        {
            Some(NativeReplay {
                provider: provider.into(),
                model: model.into(),
                wire,
                items: self.native.into_values().collect(),
            })
        } else {
            None
        };
        Ok(Completion {
            status,
            replay,
            tokens,
            call_id: String::new(),
            text: self.text,
            reasoning: self.reasoning.into_values().collect(),
            tool_calls,
            stop_reason: self.stop_reason,
        })
    }
}
fn merge_detail(details: &mut Vec<Value>, delta: &Value) {
    let same = |d: &Value| {
        if d["type"] != delta["type"] && !delta["type"].is_null() {
            return false;
        }
        if let (Some(a), Some(b)) = (d["id"].as_str(), delta["id"].as_str()) {
            return a == b;
        }
        if let (Some(a), Some(b)) = (d["index"].as_u64(), delta["index"].as_u64()) {
            return a == b;
        }
        true // continuations may omit identity; caller already selected the logical block
    };
    if let Some(existing) = details.iter_mut().find(|d| same(d)) {
        if let Some(map) = delta.as_object() {
            for (key, value) in map {
                if matches!(key.as_str(), "text" | "summary" | "data" | "signature")
                    && value.is_string()
                {
                    let mut text = existing[key].as_str().unwrap_or("").to_owned();
                    text.push_str(value.as_str().unwrap());
                    existing[key] = json!(text);
                } else if !value.is_null() {
                    existing[key] = value.clone();
                }
            }
        }
    } else {
        details.push(delta.clone());
    }
}

/// Thinking budget split adapted from FlyCockpit's Cockpit: 25/50/75/85%,
/// reserving >=1024 tokens (or 20%) for the answer. Common max maps to xhigh.
fn anthropic_budget(max_tokens: u64, effort: &str) -> Result<u64> {
    let percent = match effort {
        "low" => 25,
        "medium" => 50,
        "high" => 75,
        "max" | "xhigh" => 85,
        _ => return Err(EngineError::bad("unsupported manual Anthropic effort")),
    };
    let reserve = max_tokens.div_ceil(5).max(1024);
    let cap = max_tokens.saturating_sub(reserve);
    if cap < 1024 {
        return Err(EngineError::bad(
            "Anthropic thinking requires maxOutputTokens >= 2048",
        ));
    }
    Ok(
        ((max_tokens / 100) * percent + ((max_tokens % 100) * percent) / 100)
            .max(1024)
            .min(cap),
    )
}

/// Binary payload size is not a text token count. Reserve media context until
/// actual provider usage calibrates the session meter; keep request bytes intact.
fn count_request_tokens(bytes: &[u8]) -> u64 {
    let encoded = bytes
        .windows(8)
        .any(|w| w == b";base64," || w == b"\"base64\"")
        || bytes
            .windows(13)
            .any(|w| w == b"\"input_audio\"" || w == b"\"input_video\"")
        || bytes.windows(11).any(|w| w == b"\"video_url\"");
    if !encoded {
        return count_tokens(bytes);
    }
    let Ok(mut body) = serde_json::from_slice::<Value>(bytes) else {
        return count_tokens(bytes);
    };
    fn strip(value: &mut Value, reservation: &mut u64) {
        let kind = value["type"].as_str().unwrap_or("");
        let binary = match kind {
            "image" | "document" => value["source"]["type"] == "base64",
            "input_image" | "image_url" | "input_file" | "file" | "input_audio" | "video_url"
            | "input_video" => true,
            _ => false,
        };
        if binary {
            *reservation += if matches!(kind, "document" | "input_file" | "file") {
                8192
            } else if matches!(kind, "video_url" | "input_video") {
                16384
            } else {
                4096
            };
            *value = json!({"type":kind,"data":"[binary attachment]"});
            return;
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    strip(item, reservation);
                }
            }
            Value::Object(fields) => {
                for field in fields.values_mut() {
                    strip(field, reservation);
                }
            }
            _ => {}
        }
    }
    let mut reservation = 0;
    strip(&mut body, &mut reservation);
    count_tokens(&serde_json::to_vec(&body).unwrap_or_default()) + reservation
}

/// Tokenizer fallback for local context additions; reported input calibrates the next meter.
pub fn count_tokens(bytes: &[u8]) -> u64 {
    static ENCODER: std::sync::OnceLock<tiktoken_rs::CoreBPE> = std::sync::OnceLock::new();
    ENCODER
        .get_or_init(|| tiktoken_rs::cl100k_base().expect("bundled cl100k vocabulary"))
        .encode_with_special_tokens(&String::from_utf8_lossy(bytes))
        .len() as u64
}

pub(crate) fn history_hash(history: &[HistoryEvent]) -> String {
    use sha2::{Digest, Sha256};
    crate::hex::hex(&Sha256::digest(
        serde_json::to_vec(history).expect("history serialization"),
    ))
}

#[cfg(test)]
mod tests {
    use super::count_tokens;
    use futures_util::StreamExt;

    #[test]
    fn json_and_sse_error_predicates_accept_null_errors() {
        use super::*;
        let body: Value =
            serde_json::from_str(r#"{"status":"completed","error":null,"output":[]}"#).unwrap();
        assert!(!is_provider_error(&body));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = State::default();
        state
            .body(ProviderType::OpenaiResponses, &body, &tx)
            .unwrap();
        assert!(state.done);

        let mut parser = sse::Parser::default();
        let frames = parser.push(b"data: {\"type\":\"response.completed\",\"error\":null,\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        let event: Value = serde_json::from_str(&frames[0]).unwrap();
        assert!(!is_provider_error(&event));
        let mut state = State::default();
        state
            .event(ProviderType::OpenaiResponses, &event, &tx)
            .unwrap();
        assert!(state.done);
        for error in [
            json!({"error":{"message":"failed"}}),
            json!({"type":"error","error":null}),
        ] {
            assert!(is_provider_error(&error));
        }
    }

    #[test]
    fn chat_json_tool_calls_without_indices_remain_separate() {
        use super::*;
        let body = json!({"choices":[{"message":{"tool_calls":[
            {"id":"first","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}},
            {"id":"second","type":"function","function":{"name":"write","arguments":"{\"path\":\"b\"}"}}
        ]},"finish_reason":"tool_calls"}]});
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = State::default();
        state.body(ProviderType::OpenaiChat, &body, &tx).unwrap();
        assert!(state.done);
        assert_eq!(state.calls.len(), 2);
        for (index, id, name, args) in [
            (0, "first", "read", r#"{"path":"a"}"#),
            (1, "second", "write", r#"{"path":"b"}"#),
        ] {
            let call = &state.calls[&index];
            assert_eq!(
                (call.id.as_str(), call.name.as_str(), call.args.as_str()),
                (id, name, args)
            );
        }
        assert!(
            body["choices"][0]["message"]["tool_calls"][0]
                .get("index")
                .is_none()
        );
    }

    #[test]
    fn chat_json_preserves_existing_tool_call_indices() {
        use super::*;
        let body = json!({"choices":[{"message":{"tool_calls":[
            {"index":4,"id":"first","function":{"name":"read","arguments":"{}"}},
            {"index":7,"id":"second","function":{"name":"write","arguments":"{}"}}
        ]},"finish_reason":"tool_calls"}]});
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = State::default();
        state.body(ProviderType::OpenaiChat, &body, &tx).unwrap();
        assert_eq!(state.calls.keys().copied().collect::<Vec<_>>(), vec![4, 7]);
    }

    #[test]
    fn cl100k_token_counts_keep_bundled_encoding_and_special_tokens() {
        assert_eq!(count_tokens(b""), 0);
        assert_eq!(count_tokens(b"hello world"), 2);
        assert_eq!(count_tokens(b"<|endoftext|>"), 1);
        assert_eq!(count_tokens(&[0xff]), count_tokens("�".as_bytes()));
    }

    #[test]
    fn rustls_client_builds_https_json_and_query_requests() {
        let client = crate::http_client(|| reqwest::Client::builder().no_proxy()).unwrap();
        let request = client
            .post("https://example.invalid/model")
            .query(&[("q", "a & b")])
            .json(&serde_json::json!({"stream": true}))
            .build()
            .unwrap();
        assert_eq!(request.url().query(), Some("q=a+%26+b"));
        assert_eq!(
            request.headers()[reqwest::header::CONTENT_TYPE],
            "application/json"
        );
        assert_eq!(
            request.body().unwrap().as_bytes().unwrap(),
            br#"{"stream":true}"#
        );
    }

    pub(super) fn request_value() -> serde_json::Value {
        serde_json::json!({"prefix":"{}", "body":"{}", "wire":"command", "cache":null,
            "history_items":0, "offered_tools":[], "capability_catalog":[], "session_id":"session",
            "cache_key":"family", "conversation_id":"session:root", "agent_id":"root", "kind":"agent",
            "input_estimate":0, "retry_limit":0})
    }

    #[test]
    fn child_checkpoints_require_primary_settings_and_preserve_active_settings() {
        let value = serde_json::json!({
            "providerId":"fallback", "primaryConfig":{"maxOutputTokens":100},
            "primaryEffort":null, "primaryCaching":"off", "request":request_value(),
            "system":"system", "compactedEstimate":null, "warnedContext":false,
            "config":{"maxOutputTokens":200}, "effort":"high", "caching":"auto",
            "historyLen":0, "historyHash":"hash", "context":1
        });
        let checkpoint: super::ChildRequest = serde_json::from_value(value).unwrap();
        let saved = serde_json::to_value(checkpoint).unwrap();
        assert_eq!(saved["primaryConfig"]["maxOutputTokens"], 100);
        assert_eq!(saved["config"]["maxOutputTokens"], 200);
        assert!(saved["primaryEffort"].is_null());
        assert_eq!(saved["effort"], "high");
        assert_eq!(saved["primaryCaching"], "off");
        assert_eq!(saved["caching"], "auto");
        let restored: super::ChildRequest = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), saved);
        for field in ["primaryConfig", "primaryCaching"] {
            let mut missing = saved.clone();
            missing.as_object_mut().unwrap().remove(field);
            let error = serde_json::from_value::<super::ChildRequest>(missing).unwrap_err();
            assert!(error.to_string().contains(field), "{field}: {error}");
            let mut null = saved.clone();
            null[field] = serde_json::Value::Null;
            assert!(serde_json::from_value::<super::ChildRequest>(null).is_err());
        }
    }

    #[test]
    fn saved_requests_require_and_preserve_conversation_identity() {
        let value = request_value();
        let request: super::PreparedRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(request.cache_identity(), "family");
        assert_eq!(request.conversation_identity(), "session:root");
        let saved = serde_json::to_value(request).unwrap();
        let replay: super::PreparedRequest = serde_json::from_value(saved).unwrap();
        assert_eq!(replay.conversation_identity(), "session:root");
        for identity in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!("  "),
        ] {
            let mut invalid = value.clone();
            invalid["conversation_id"] = identity;
            assert!(serde_json::from_value::<super::PreparedRequest>(invalid).is_err());
        }
        let mut missing = value;
        missing.as_object_mut().unwrap().remove("conversation_id");
        assert!(serde_json::from_value::<super::PreparedRequest>(missing).is_err());
    }

    #[tokio::test]
    async fn provider_error_prefix_caps_chunks_and_stops_polling() {
        let chunks = futures_util::stream::iter([
            Ok::<_, ()>(vec![b'a'; super::ERROR_BODY_LIMIT - 1]),
            Ok(vec![b'b'; 100]),
        ])
        .chain(futures_util::stream::poll_fn(
            |_| -> std::task::Poll<Option<Result<Vec<u8>, ()>>> {
                panic!("stream polled after prefix was full")
            },
        ));
        let body = super::error_body_prefix(chunks).await.unwrap();
        assert_eq!(body.len(), super::ERROR_BODY_LIMIT);
        assert!(body.ends_with('b'));
    }

    #[tokio::test]
    async fn provider_error_prefix_preserves_small_utf8_and_empty_bodies() {
        for text in ["", "provider error", "échec 🌍"] {
            let stream = futures_util::stream::iter([Ok::<_, ()>(text.as_bytes())]);
            assert_eq!(super::error_body_prefix(stream).await.unwrap(), text);
        }
    }

    #[tokio::test]
    async fn provider_error_prefix_handles_split_utf8_boundary() {
        let prefix = "a".repeat(super::ERROR_BODY_LIMIT - 1);
        let stream =
            futures_util::stream::iter([Ok::<_, ()>(prefix.as_bytes()), Ok("🌍".as_bytes())]);
        let body = super::error_body_prefix(stream).await.unwrap();
        assert_eq!(body, prefix);
        let invalid = futures_util::stream::iter([Ok::<_, ()>(vec![0xff])]);
        assert_eq!(super::error_body_prefix(invalid).await.unwrap(), "�");
    }

    #[tokio::test]
    async fn provider_error_prefix_reports_stream_errors() {
        let stream = futures_util::stream::iter([Ok(b"partial".as_slice()), Err("read error")]);
        assert_eq!(
            super::error_body_prefix(stream).await.unwrap_err(),
            "read error"
        );
    }
}
