//! User-owned JSON configuration. Resolved connection values are deliberately not serializable.
pub mod discovery;
pub mod goal;
pub mod lsp;
pub mod tools;
pub use goal::{GoalConfig, GoalLimits, UsageResume};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    time::SystemTime,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("HOME is not set")]
    NoHome,
    #[error("environment variable {0} is not set")]
    MissingVariable(String),
    #[error("{path}: {message}")]
    File { path: PathBuf, message: String },
    #[error("{0}")]
    Invalid(String),
}
type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Clone, Debug, Serialize)]
pub struct Directories {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
}
impl Directories {
    /// Resolve once at startup. No directories are created as a side effect.
    pub fn resolve() -> Result<Self> {
        if let Some(root) = env::var_os("COCKPIT_CODE_HOME").filter(|s| !s.is_empty()) {
            let root = PathBuf::from(root);
            return Ok(Self {
                config: root.clone(),
                data: root.clone(),
                cache: root.join("cache"),
            });
        }
        let home = PathBuf::from(env::var_os("HOME").ok_or(ConfigError::NoHome)?);
        let xdg = home.join(".config").is_dir();
        let fallback = home.join(".cockpit-code");
        let resolve = |var: &str, default: &str, fallback: PathBuf| {
            env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join("cockpit-code"))
                .unwrap_or_else(|| {
                    if xdg {
                        home.join(default).join("cockpit-code")
                    } else {
                        fallback
                    }
                })
        };
        Ok(Self {
            config: resolve("XDG_CONFIG_HOME", ".config", fallback.clone()),
            data: resolve("XDG_DATA_HOME", ".local/share", fallback.clone()),
            cache: resolve("XDG_CACHE_HOME", ".cache", fallback.join("cache")),
        })
    }
}
fn yes() -> bool {
    true
}
fn four() -> usize {
    4
}
fn three() -> usize {
    3
}
fn one() -> usize {
    1
}
fn first_timeout() -> u64 {
    120
}
fn idle_timeout() -> u64 {
    300
}
fn startup_timeout() -> u64 {
    30
}
fn skills() -> Vec<String> {
    vec!["~/.agents/skills".into(), "./.agents/skills".into()]
}
fn theme() -> String {
    "auto".into()
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Manual,
    Auto,
    Unattended,
    Yolo,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Permissions {
    pub reviewer_guidance: Option<String>,
    #[serde(skip)]
    pub resolved_reviewer_guidance: Option<String>,
    pub mode: PermissionMode,
    pub auto_model: Option<String>,
    pub auto_variations: usize,
    pub prompts: PermissionPrompts,
}
impl Default for Permissions {
    fn default() -> Self {
        Self {
            reviewer_guidance: None,
            resolved_reviewer_guidance: None,
            mode: PermissionMode::Manual,
            auto_model: None,
            auto_variations: three(),
            prompts: PermissionPrompts::default(),
        }
    }
}
/// Markdown replacements for the built-in safety classifier prompts.
/// Relative paths are resolved against the directory containing config.json.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PermissionPrompts {
    pub tool_call: Option<PathBuf>,
    pub write_edit: Option<PathBuf>,
    #[serde(skip)]
    pub resolved_tool_call: Option<String>,
    #[serde(skip)]
    pub resolved_write_edit: Option<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentMode {
    #[default]
    Tree,
    Flat,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Subagents {
    pub mode: SubagentMode,
    pub max_depth: Option<usize>,
    pub max_concurrent_children: usize,
    pub max_total_agents: Option<usize>,
}
impl Default for Subagents {
    fn default() -> Self {
        Self {
            mode: SubagentMode::Tree,
            max_depth: None,
            max_concurrent_children: four(),
            max_total_agents: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Benchmark {
    pub label: String,
    pub unit: Option<String>,
    #[serde(default = "yes")]
    pub higher_is_better: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub tools: tools::Profiles,
    pub subscribe: Vec<PathBuf>,
    pub shared_paths: Vec<String>,
    pub guidance: GuidanceConfig,
    pub goal: GoalConfig,
    pub default_model: Option<String>,
    pub permissions: Permissions,
    pub subagents: Subagents,
    pub script: ScriptConfig,
    /// Limits for the `generate` tool's provider output.
    pub generate: GenerateConfig,
    pub lsp: lsp::LspConfig,
    pub benchmarks: BTreeMap<String, Benchmark>,
    pub system_prompt: Option<PathBuf>,
    #[serde(skip)]
    pub resolved_system_prompt: Option<String>,
    pub skills_dirs: Vec<String>,
    pub theme: String,
    /// TUI mouse capture default (F12 and `/mouse` toggle it at runtime).
    pub mouse: bool,
    /// TUI attention signal when a prompt waits or a turn ends: `bell`
    /// (default: BEL, which tmux flags, plus a title marker), `desktop`
    /// (also an OSC 9 / OSC 777 desktop notification) or `off`.
    pub attention: String,
    /// Warn before switching the root model when the last message is younger
    /// than this many minutes (its prompt cache is probably warm). 0 is off.
    pub model_switch_warn_minutes: u64,
    /// TUI composer vim mode (`/vim` toggles it for the session).
    pub vim: bool,
    /// TUI: a paste of at least this many lines (or over 800 characters)
    /// collapses into a paste chip.
    pub paste_collapse_lines: usize,
    /// TUI: the class a message typed while the agent works is queued
    /// with: `next-turn` (default), `steer` or `interrupt`.
    pub queue_default: QueueClass,
    /// Minimum foreground runtime before a pending steer backgrounds it.
    pub steer_background_secs: u64,
    /// Minimum time a steer waits before backgrounding a foreground call.
    pub steer_min_wait_secs: u64,
    /// Web tool provider. `apiKey` is expanded (`${VAR}`) at call time only.
    pub web: WebConfig,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GuidanceConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub max_items: usize,
}
impl Default for GuidanceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            max_items: 5,
        }
    }
}
/// `web_search` needs a provider; `web_fetch` falls back to a built-in GET.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WebConfig {
    pub provider: Option<WebProvider>,
    /// Normally `"${FIRECRAWL_API_KEY}"` or `"${TINYFISH_API_KEY}"`; never a literal key.
    pub api_key: Option<String>,
    /// Optional endpoint override (self-hosted Firecrawl, or a local mock).
    /// For TinyFish it replaces both the search and fetch hosts.
    pub base_url: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WebProvider {
    Firecrawl,
    Tinyfish,
}
/// How a queued user message is delivered.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum QueueClass {
    /// Held until the agent's turn ends (root) or its next task (child).
    #[default]
    NextTurn,
    /// Delivered at the next model boundary; a blocking call moves to the background.
    Steer,
    /// Stops the in-flight generation, then is delivered at once.
    Interrupt,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            tools: tools::Profiles::default(),
            subscribe: Vec::new(),
            shared_paths: [
                "target/",
                "node_modules/",
                ".git/",
                "Cargo.lock",
                "package-lock.json",
                "yarn.lock",
                "pnpm-lock.yaml",
                "bun.lock",
                "bun.lockb",
                "poetry.lock",
                "uv.lock",
                "Pipfile.lock",
                "Gemfile.lock",
                "composer.lock",
                "go.sum",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            guidance: GuidanceConfig::default(),
            goal: GoalConfig::default(),
            default_model: None,
            permissions: Permissions::default(),
            subagents: Subagents::default(),
            script: ScriptConfig::default(),
            generate: GenerateConfig::default(),
            lsp: lsp::LspConfig::default(),
            benchmarks: BTreeMap::new(),
            system_prompt: None,
            resolved_system_prompt: None,
            skills_dirs: skills(),
            theme: theme(),
            mouse: true,
            attention: "bell".into(),
            model_switch_warn_minutes: 60,
            vim: false,
            paste_collapse_lines: 8,
            queue_default: QueueClass::NextTurn,
            steer_background_secs: 60,
            steer_min_wait_secs: 15,
            web: WebConfig::default(),
        }
    }
}
impl Config {
    pub fn higher_is_better(&self, key: &str) -> bool {
        self.benchmarks.get(key).is_none_or(|b| b.higher_is_better)
    }
    /// The configured prompt for reviewing a write/edit (`writes`) or another
    /// tool call, read from its file at load.
    pub fn review_prompt(&self, writes: bool) -> Option<&str> {
        let prompts = &self.permissions.prompts;
        if writes {
            prompts.resolved_write_edit.as_deref()
        } else {
            prompts.resolved_tool_call.as_deref()
        }
    }
    /// The global reviewer guidance text, inline or read from its file at load.
    pub fn reviewer_guidance(&self) -> Option<&str> {
        self.permissions.resolved_reviewer_guidance.as_deref()
    }
    /// The limits `config.json` must respect beyond its schema.
    fn validate(&self) -> Result<()> {
        if self.lsp.after_write_ms == 0 {
            return Err(ConfigError::Invalid(
                "lsp.afterWriteMs must be positive".into(),
            ));
        }
        if !(1..=5).contains(&self.guidance.max_items) {
            return Err(ConfigError::Invalid("guidance.maxItems must be 1–5".into()));
        }
        validate_web(&self.web)?;
        if self.generate.max_output_bytes == 0 {
            return Err(ConfigError::Invalid(
                "generate.maxOutputBytes must be positive".into(),
            ));
        }
        Ok(())
    }
    /// The prompt files this configuration reads, resolved against `root`.
    /// Reviewer guidance counts only when it names an existing file.
    fn prompt_files(&self, root: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = [
            self.system_prompt.as_ref(),
            self.permissions.prompts.tool_call.as_ref(),
            self.permissions.prompts.write_edit.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(|path| prompt_path(root, path))
        .collect();
        if let Some(raw) = &self.permissions.reviewer_guidance {
            let path = prompt_path(root, Path::new(raw));
            if path.is_file() {
                paths.push(path);
            }
        }
        paths
    }
    /// Read the prompt files into their resolved texts; `system.md` is the
    /// default system prompt.
    fn resolve_prompts(&mut self, root: &Path) -> Result<()> {
        if self.system_prompt.is_none() && root.join("system.md").exists() {
            self.system_prompt = Some(PathBuf::from("system.md"));
        }
        if let Some(path) = &self.system_prompt {
            let (path, text) = read_prompt(root, path)?;
            self.system_prompt = Some(path);
            self.resolved_system_prompt = Some(text);
        }
        if let Some(raw) = &self.permissions.reviewer_guidance {
            let path = prompt_path(root, Path::new(raw));
            self.permissions.resolved_reviewer_guidance = Some(
                if path.is_file()
                    || (!raw.contains('\n')
                        && (raw.ends_with(".md") || raw.starts_with("./") || raw.starts_with('/')))
                {
                    read_prompt(root, Path::new(raw))?.1
                } else {
                    raw.clone()
                },
            );
        }
        let prompts = &mut self.permissions.prompts;
        if let Some(path) = &prompts.tool_call {
            let (path, text) = read_prompt(root, path)?;
            prompts.tool_call = Some(path);
            prompts.resolved_tool_call = Some(text);
        }
        if let Some(path) = &prompts.write_edit {
            let (path, text) = read_prompt(root, path)?;
            prompts.write_edit = Some(path);
            prompts.resolved_write_edit = Some(text);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderType {
    Anthropic,
    OpenaiChat,
    OpenaiResponses,
    Command,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PromptCaching {
    #[default]
    Auto,
    #[serde(rename = "1h")]
    OneHour,
    Off,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Timeouts {
    pub first_token_secs: u64,
    pub idle_secs: u64,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            first_token_secs: first_timeout(),
            idle_secs: idle_timeout(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Reasoning {
    pub efforts: Vec<String>,
    pub default: Option<String>,
    #[serde(default)]
    pub normalized: BTreeMap<String, String>,
    /// Optional manual thinking budgets, keyed by vendor effort name.
    #[serde(default)]
    pub budgets: BTreeMap<String, u64>,
}
impl Reasoning {
    pub fn resolve(&self, requested: Option<&str>) -> Result<Option<String>> {
        let Some(effort) = requested.or(self.default.as_deref()) else {
            return Ok(None);
        };
        let vendor = if requested.is_none() {
            // Config defaults are vendor names; normalization applies to requested common names.
            effort
        } else {
            self.normalized
                .get(effort)
                .map(String::as_str)
                .unwrap_or(effort)
        };
        if !self.efforts.iter().any(|e| e == vendor) {
            return Err(ConfigError::Invalid(format!(
                "unsupported effort {effort}; supported: {}",
                self.efforts.join(", ")
            )));
        }
        Ok(Some(vendor.into()))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Text,
    Image,
    Video,
    Audio,
    Pdf,
}
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    /// The original media modalities. Generic file support is in `file_input`.
    pub input: Vec<Modality>,
    /// Serialized as `"file"` in capabilities.input, never as a separate field.
    pub file_input: bool,
    pub output: Vec<Modality>,
    pub tools: bool,
}
// Keep the existing Modality enum exhaustive for frontend consumers. Generic
// files are an input capability, not a generated-media/output modality.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct CapabilitiesJson {
    input: Vec<String>,
    output: Vec<Modality>,
    tools: bool,
}
impl Serialize for Capabilities {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut input = self
            .input
            .iter()
            .map(|m| {
                serde_json::to_value(m)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        if self.file_input {
            input.push("file".into());
        }
        CapabilitiesJson {
            input,
            output: self.output.clone(),
            tools: self.tools,
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Capabilities {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = CapabilitiesJson::deserialize(deserializer)?;
        let mut input = Vec::new();
        let mut file_input = false;
        for kind in raw.input {
            if kind == "file" {
                file_input = true;
            } else {
                input.push(
                    serde_json::from_value(serde_json::Value::String(kind))
                        .map_err(serde::de::Error::custom)?,
                );
            }
        }
        Ok(Self {
            input,
            file_input,
            output: raw.output,
            tools: raw.tools,
        })
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub input_per_m_tok: Option<f64>,
    pub output_per_m_tok: Option<f64>,
    pub cache_read_per_m_tok: Option<f64>,
    pub cache_write_per_m_tok: Option<f64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Billing {
    #[default]
    Usage,
    Subscription,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    Adaptive,
    Manual,
    None,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PriorThinking {
    #[default]
    Keep,
    Drop,
    Summarize,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostUnit {
    Usd,
    UsdMicros,
}
impl CostUnit {
    pub fn dollars(self, value: f64) -> Option<f64> {
        let value = match self {
            Self::Usd => value,
            Self::UsdMicros => value / 1_000_000.0,
        };
        (value.is_finite() && value >= 0.0).then_some(value)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "lowercase")]
pub enum ReportedCost {
    Body { pointer: String, unit: CostUnit },
    Header { name: String, unit: CostUnit },
}
impl ReportedCost {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Body { pointer, .. } if !pointer.is_empty() && !pointer.starts_with('/') => Err(
                ConfigError::Invalid("reportedCost pointer must be a JSON pointer".into()),
            ),
            Self::Header { name, .. }
                if name.is_empty()
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) =>
            {
                Err(ConfigError::Invalid(
                    "invalid reportedCost header name".into(),
                ))
            }
            _ => Ok(()),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    /// Ordered backup refs, tried for the current turn only.
    #[serde(default)]
    pub fallback: Vec<String>,
    pub lsp: Option<lsp::LspOverrides>,
    #[serde(default)]
    pub tools: tools::Settings,
    pub autocompact_at_idle: Option<u64>,
    pub autocompact_at_idle_min_percent: Option<u64>,
    pub model_prompt: Option<String>,
    pub model_reminder: Option<String>,
    pub model_reminder_turns: Option<u64>,
    pub cache_ttl_secs: Option<u64>,
    pub cold_resume_min_tokens: Option<u64>,
    pub max_concurrent_contexts: Option<usize>,
    #[serde(rename = "type")]
    pub kind: Option<ProviderType>,
    pub thinking: Option<ThinkingMode>,
    #[serde(default)]
    pub prior_thinking: PriorThinking,
    pub thinking_summary_model: Option<String>,
    #[serde(default)]
    pub text_tool_recovery: bool,
    pub reported_cost: Option<ReportedCost>,
    pub strict_tools: Option<bool>,
    pub label: Option<String>,
    #[serde(default)]
    pub description: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning: Reasoning,
    #[serde(default)]
    pub capabilities: Capabilities,
    pub allow_in_subagents: Option<bool>,
    pub cost: Option<Cost>,
    #[serde(default)]
    pub benchmarks: BTreeMap<String, f64>,
    #[serde(default)]
    pub billing: Billing,
    pub compact_at: Option<f64>,
    /// Root notice only; a fraction of its context window (default .75).
    pub root_compact_suggest_at: Option<f64>,
    #[serde(default = "one")]
    pub self_evaluate_writes: usize,
    #[serde(default)]
    pub variation_focuses: Vec<String>,
    /// Media models only: how `generate` asks for an image.
    #[serde(default)]
    pub image_request: ImageRequest,
    /// Media models only: escape hatch; JSON merged last into the `generate`
    /// request body (its keys override the `imageRequest` defaults).
    pub generate_params: Option<serde_json::Value>,
}
/// `generate` image request style for a media model.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageRequest {
    /// `{"modalities": ["image", "text"]}`: models with native image output
    /// (OpenRouter's form).
    #[default]
    Modalities,
    /// `{"tools": [{"type": "image_generation"}]}`: OpenAI's hosted image tool.
    Tool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub argv: Vec<String>,
    pub ttl_secs: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCommand {
    pub argv: Vec<String>,
    pub ttl_secs: Option<u64>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetPeriod {
    Day,
    Week,
    Month,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Budget {
    pub usd: f64,
    pub period: BudgetPeriod,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandConfig {
    pub argv: Vec<String>,
    pub stdin: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub output: Option<String>,
    pub history: Option<String>,
    pub timeout_secs: Option<u64>,
    pub discover: Option<Discovery>,
}

/// Expanded credentials have no Serialize or Debug implementation.
#[derive(Clone)]
pub struct ResolvedCommand {
    pub argv: Vec<String>,
    pub stdin: Option<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub output: Option<String>,
    pub history: Option<String>,
    pub timeout_secs: Option<u64>,
    pub discover_argv: Option<Vec<String>>,
    /// Credential values substituted from environment references, for diagnostic redaction.
    pub redaction_values: Vec<String>,
}
#[derive(Clone)]
pub struct ResolvedMcp {
    pub transport: Transport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub timeouts: McpTimeouts,
}
// No Debug implementation: expanded URLs, env and headers must never enter debug logs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    #[serde(default)]
    pub tools: tools::Settings,
    pub root_compact_suggest_at: Option<f64>,
    pub autocompact_at_idle: Option<u64>,
    pub autocompact_at_idle_min_percent: Option<u64>,
    pub model_prompt: Option<String>,
    pub model_reminder: Option<String>,
    pub model_reminder_turns: Option<u64>,
    pub cache_ttl_secs: Option<u64>,
    pub cold_resume_min_tokens: Option<u64>,
    pub max_concurrent_contexts: Option<usize>,
    #[serde(rename = "type")]
    pub kind: ProviderType,
    pub label: Option<String>,
    pub base_url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "yes")]
    pub allow_in_subagents: bool,
    #[serde(default)]
    pub prompt_caching: PromptCaching,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    pub command: Option<CommandConfig>,
    pub reported_cost: Option<ReportedCost>,
    pub strict_tools: Option<bool>,
    pub usage: Option<UsageCommand>,
    pub budget: Option<Budget>,
    #[serde(skip)]
    resolved_usage_argv: Option<Vec<String>>,
    #[serde(skip)]
    resolved_url: Option<String>,
    #[serde(skip)]
    resolved_headers: BTreeMap<String, String>,
    #[serde(skip)]
    resolved_command: Option<ResolvedCommand>,
}
impl Provider {
    pub fn connection_url(&self) -> Option<&str> {
        self.resolved_url.as_deref()
    }
    pub fn connection_headers(&self) -> &BTreeMap<String, String> {
        &self.resolved_headers
    }
    pub fn connection_command(&self) -> Option<&ResolvedCommand> {
        self.resolved_command.as_ref()
    }
    pub fn usage_argv(&self) -> Option<&[String]> {
        self.resolved_usage_argv.as_deref()
    }
    pub fn strict_tools(&self, model: &ModelConfig) -> bool {
        model
            .strict_tools
            .or(self.strict_tools)
            .unwrap_or(model.kind.unwrap_or(self.kind) == ProviderType::OpenaiResponses)
    }
    /// Overlay the provider's tool overrides, then those of its model `model`, onto `out`.
    fn overlay_tool_overrides(&self, model: &str, out: &mut BTreeMap<String, tools::ToolOverride>) {
        for layer in [
            Some(&self.tools.overrides),
            self.models.get(model).map(|c| &c.tools.overrides),
        ]
        .into_iter()
        .flatten()
        {
            for (name, config) in layer {
                out.entry(name.clone()).or_default().overlay(config);
            }
        }
    }
    /// The tool profile of `model`, else of this provider, else `global`.
    fn tool_profile(&self, model: Option<&ModelConfig>, global: tools::Profile) -> tools::Profile {
        let inherited = self.tools.profile.unwrap_or(global);
        model.map_or(inherited, |m| m.tools.profile.unwrap_or(inherited))
    }
    /// Where `model`'s responses report their cost: its own locator, else the provider's.
    pub fn reported_cost<'a>(&'a self, model: &'a ModelConfig) -> Option<&'a ReportedCost> {
        model.reported_cost.as_ref().or(self.reported_cost.as_ref())
    }
    fn model(&self, id: &str) -> Option<&ModelConfig> {
        self.models.get(id)
    }
    /// The configured and discovered models, by ID.
    fn models(&self) -> impl Iterator<Item = (&String, &ModelConfig)> {
        self.models.iter()
    }
    /// Every model is billed by subscription (and there is at least one).
    pub fn is_subscription(&self) -> bool {
        !self.models.is_empty()
            && self
                .models
                .values()
                .all(|m| matches!(m.billing, Billing::Subscription))
    }
    /// The command that lists this provider's models, if it discovers them.
    pub fn discovery(&self) -> Option<&Discovery> {
        self.command.as_ref().and_then(|c| c.discover.as_ref())
    }
    /// Whether the provider has a command block (it runs or discovers through an adapter).
    pub fn has_command(&self) -> bool {
        self.command.is_some()
    }
    /// Whether `model` runs as a command that is only given the latest turn
    /// (`history: latest`), so the harness cannot know its context use.
    pub fn latest_history_only(&self, model: &ModelConfig) -> bool {
        model.kind.unwrap_or(self.kind) == ProviderType::Command
            && self
                .command
                .as_ref()
                .is_some_and(|c| c.history.as_deref() == Some("latest"))
    }
    /// Whether `model` runs as a command that answers in plain text (the default output).
    pub fn text_command(&self, model: &ModelConfig) -> bool {
        model.kind.unwrap_or(self.kind) == ProviderType::Command
            && self
                .command
                .as_ref()
                .is_some_and(|c| c.output.as_deref().unwrap_or("text") == "text")
    }
    /// The resolved environment of the provider's command, shared by its usage command.
    pub fn command_env(&self) -> Option<&BTreeMap<String, String>> {
        self.resolved_command.as_ref().map(|c| &c.env)
    }
    /// This provider as `model` runs it: the model's dialect wins, and a
    /// command without an explicit limit runs one context at a time.
    pub fn for_model(mut self, model: &ModelConfig) -> Self {
        self.kind = model.kind.unwrap_or(self.kind);
        if self.kind == ProviderType::Command && self.max_concurrent_contexts.is_none() {
            self.max_concurrent_contexts = Some(1);
        }
        self
    }
    /// A copy that caches prompts as a frozen model did when it was chosen.
    pub fn with_caching(&self, caching: PromptCaching) -> Self {
        let mut provider = self.clone();
        provider.prompt_caching = caching;
        provider
    }
    /// How long a usage reading stays fresh (`usage.ttlSecs`, 300 by
    /// default); `None` when no usage command reports this provider's quota.
    pub fn usage_ttl_secs(&self) -> Option<u64> {
        self.usage.as_ref().map(|u| u.ttl_secs.unwrap_or(300))
    }
    /// The soft spending budget, if one is set.
    pub fn budget(&self) -> Option<Budget> {
        self.budget.clone()
    }
    fn resolve(&mut self) -> Result<()> {
        if self
            .budget
            .as_ref()
            .is_some_and(|b| !b.usd.is_finite() || b.usd <= 0.0)
        {
            return Err(ConfigError::Invalid(
                "budget.usd must be positive and finite".into(),
            ));
        }
        self.resolved_usage_argv = self
            .usage
            .as_ref()
            .map(|u| {
                if u.argv.is_empty() || u.argv[0].is_empty() {
                    return Err(ConfigError::Invalid(
                        "usage.argv must name an executable".into(),
                    ));
                }
                u.argv.iter().map(|s| expand(s)).collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        if self.max_concurrent_contexts == Some(0)
            || self
                .models
                .values()
                .any(|m| m.max_concurrent_contexts == Some(0))
        {
            return Err(ConfigError::Invalid(
                "maxConcurrentContexts must be positive".into(),
            ));
        }
        if self.kind == ProviderType::Command
            || self
                .models
                .values()
                .any(|m| m.kind == Some(ProviderType::Command))
        {
            let c = self.command.as_ref().ok_or_else(|| {
                ConfigError::Invalid("command provider requires command block".into())
            })?;
            if c.argv.is_empty()
                || c.argv[0].is_empty()
                || c.timeout_secs == Some(0)
                || c.output
                    .as_deref()
                    .is_some_and(|s| !["text", "jsonl"].contains(&s))
                || c.history
                    .as_deref()
                    .is_some_and(|s| !["full", "latest"].contains(&s))
                || c.discover.as_ref().is_some_and(|d| d.argv.is_empty())
            {
                return Err(ConfigError::Invalid(
                    "invalid command argv, output, history, discovery or timeout".into(),
                ));
            }
        }
        self.resolved_url = self.base_url.as_deref().map(expand).transpose()?;
        self.resolved_headers = expand_map(&self.headers)?;
        self.resolved_command = self
            .command
            .as_ref()
            .map(|c| -> Result<_> {
                let mut redaction_values = Vec::new();
                let mut expand = |s: &str| {
                    expand_command_with(s, &mut redaction_values, |name| {
                        env::var(name).map_err(|_| ConfigError::MissingVariable(name.into()))
                    })
                };
                Ok(ResolvedCommand {
                    argv: c.argv.iter().map(|s| expand(s)).collect::<Result<_>>()?,
                    stdin: c.stdin.as_deref().map(&mut expand).transpose()?,
                    cwd: c.cwd.as_deref().map(&mut expand).transpose()?,
                    env: c
                        .env
                        .iter()
                        .map(|(name, value)| Ok((name.clone(), expand(value)?)))
                        .collect::<Result<_>>()?,
                    output: c.output.clone(),
                    history: c.history.clone(),
                    timeout_secs: c.timeout_secs,
                    discover_argv: c
                        .discover
                        .as_ref()
                        .map(|d| d.argv.iter().map(|s| expand(s)).collect::<Result<Vec<_>>>())
                        .transpose()?,
                    redaction_values,
                })
            })
            .transpose()?;
        if let Some(locator) = &self.reported_cost {
            locator.validate()?;
        }
        if self.kind != ProviderType::Command && self.resolved_url.is_none() {
            return Err(ConfigError::Invalid(
                "HTTP provider requires baseUrl".into(),
            ));
        }
        if self.timeouts.first_token_secs == 0 || self.timeouts.idle_secs == 0 {
            return Err(ConfigError::Invalid("timeouts must be positive".into()));
        }
        for model in self.models.values_mut() {
            model.model_prompt = model.model_prompt.clone().or(self.model_prompt.clone());
            model.model_reminder = model.model_reminder.clone().or(self.model_reminder.clone());
            model.model_reminder_turns = model
                .model_reminder_turns
                .or(self.model_reminder_turns)
                .or(Some(10));
            if model
                .lsp
                .as_ref()
                .is_some_and(|lsp| lsp.after_write_ms == Some(0))
            {
                return Err(ConfigError::Invalid(
                    "model lsp.afterWriteMs must be positive".into(),
                ));
            }
            if model.fallback.iter().any(|r| {
                r.split_once('/')
                    .is_none_or(|(p, m)| p.is_empty() || m.is_empty())
            }) {
                return Err(ConfigError::Invalid(
                    "model fallback entries must be provider/model refs".into(),
                ));
            }
            if model.model_reminder_turns == Some(0) {
                return Err(ConfigError::Invalid(
                    "modelReminderTurns must be positive".into(),
                ));
            }
            model.root_compact_suggest_at = model
                .root_compact_suggest_at
                .or(self.root_compact_suggest_at)
                .or(Some(0.75));
            model.autocompact_at_idle = model.autocompact_at_idle.or(self.autocompact_at_idle);
            model.autocompact_at_idle_min_percent = model
                .autocompact_at_idle_min_percent
                .or(self.autocompact_at_idle_min_percent)
                .or(Some(30));
            model.cold_resume_min_tokens = model
                .cold_resume_min_tokens
                .or(self.cold_resume_min_tokens)
                .or(Some(20000));
            model.cache_ttl_secs = model.cache_ttl_secs.or(self.cache_ttl_secs).or(Some(300));
            if model.autocompact_at_idle == Some(0)
                || model.cache_ttl_secs == Some(0)
                || model
                    .autocompact_at_idle_min_percent
                    .is_some_and(|p| p > 100)
            {
                return Err(ConfigError::Invalid(
                    "idle compaction/cache TTL must be positive; minimum percent must be 0..100"
                        .into(),
                ));
            }
        }
        for (model_id, model) in &self.models {
            model.reasoning.resolve(None)?;
            let dialect = model.kind.unwrap_or(self.kind);
            if dialect == ProviderType::Command
                && matches!(
                    model.reported_cost.as_ref().or(self.reported_cost.as_ref()),
                    Some(ReportedCost::Header { .. })
                )
            {
                return Err(ConfigError::Invalid(
                    "command reportedCost requires a body locator".into(),
                ));
            }
            let protected_chat = model_id.to_ascii_lowercase().contains("deepseek")
                || self.resolved_url.as_deref().is_some_and(|url| {
                    url.contains("deepseek.com") || url.contains("openrouter.ai")
                });
            if model.prior_thinking != PriorThinking::Keep
                && (matches!(
                    dialect,
                    ProviderType::Anthropic | ProviderType::OpenaiResponses
                ) || (dialect == ProviderType::OpenaiChat && protected_chat))
            {
                return Err(ConfigError::Invalid("priorThinking must be keep for signed/encrypted reasoning and providers requiring complete reasoning replay (Anthropic, Responses, DeepSeek and OpenRouter)".into()));
            }
            if model.thinking_summary_model.as_deref().is_some_and(|id| {
                id.split_once('/')
                    .is_none_or(|(p, m)| p.is_empty() || m.is_empty())
            }) {
                return Err(ConfigError::Invalid(
                    "thinkingSummaryModel must be provider/model".into(),
                ));
            }
            if model.self_evaluate_writes == 0 {
                return Err(ConfigError::Invalid(
                    "selfEvaluateWrites must be positive".into(),
                ));
            }
            if let Some(locator) = &model.reported_cost {
                locator.validate()?;
            }
            if model.compact_at.is_some_and(|n| !(0.0..=1.0).contains(&n)) {
                return Err(ConfigError::Invalid("compactAt must be in 0..1".into()));
            }
            if model
                .root_compact_suggest_at
                .is_some_and(|n| !n.is_finite() || n <= 0.0 || n > 1.0)
            {
                return Err(ConfigError::Invalid(
                    "rootCompactSuggestAt must be in (0,1]".into(),
                ));
            }
            if let Some(c) = &model.cost {
                for rate in [
                    c.input_per_m_tok,
                    c.output_per_m_tok,
                    c.cache_read_per_m_tok,
                    c.cache_write_per_m_tok,
                ]
                .into_iter()
                .flatten()
                {
                    if !rate.is_finite() || rate < 0.0 {
                        return Err(ConfigError::Invalid(
                            "cost rates must be finite and nonnegative".into(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default)]
pub struct ModelToggles {
    pub disabled_providers: BTreeSet<String>,
    pub disabled_models: BTreeSet<String>,
}
pub fn subagent_eligible(
    provider_id: &str,
    model_id: &str,
    provider: &Provider,
    model: &ModelConfig,
    toggles: &ModelToggles,
) -> bool {
    provider.enabled
        && provider.allow_in_subagents
        && model.allow_in_subagents != Some(false)
        && model.capabilities.output == [Modality::Text]
        && !toggles.disabled_providers.contains(provider_id)
        && !toggles
            .disabled_models
            .contains(&format!("{provider_id}/{model_id}"))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Stdio,
    Http,
    Sse,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpTimeouts {
    pub startup_secs: u64,
    pub call_secs: u64,
}
impl Default for McpTimeouts {
    fn default() -> Self {
        Self {
            startup_secs: startup_timeout(),
            call_secs: first_timeout(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mcp {
    pub transport: Transport,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub enabled_by_default: bool,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub timeouts: McpTimeouts,
    #[serde(skip)]
    resolved: Option<ResolvedMcp>,
}
impl Mcp {
    pub fn connection(&self) -> Option<&ResolvedMcp> {
        self.resolved.as_ref()
    }
}

pub fn expand(s: &str) -> Result<String> {
    expand_with(s, |name| {
        env::var(name).map_err(|_| ConfigError::MissingVariable(name.into()))
    })
}

/// Environment names whose values should be hidden in command diagnostics:
/// an exact `_`-separated token (or the whole name) such as `KEY`, `TOKEN` or
/// `PAT`, or a name ending in `TOKEN`, `SECRET`, `PASSWORD` or `PASSWD`
/// (`GITHUBTOKEN`, `DBPASSWORD`). Socket paths and the working-directory
/// variables are excluded.
pub fn is_credential_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    if matches!(name.as_str(), "SSH_AUTH_SOCK" | "PWD" | "OLDPWD") {
        return false;
    }
    ["TOKEN", "SECRET", "PASSWORD", "PASSWD"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
        || std::iter::once(name.as_str())
            .chain(name.split('_'))
            .any(|token| {
                matches!(
                    token,
                    "KEY"
                        | "APIKEY"
                        | "TOKEN"
                        | "SECRET"
                        | "PASSWORD"
                        | "PASSWD"
                        | "PASS"
                        | "PWD"
                        | "PAT"
                        | "CREDENTIAL"
                        | "CREDENTIALS"
                        | "AUTH"
                )
            })
}

fn expand_command_with(
    s: &str,
    redaction_values: &mut Vec<String>,
    mut lookup: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    expand_with(s, |name| {
        let value = lookup(name)?;
        if is_credential_name(name) && value.len() >= 8 {
            redaction_values.push(value.clone());
        }
        Ok(value)
    })
}

fn expand_with(s: &str, mut lookup: impl FnMut(&str) -> Result<String>) -> Result<String> {
    let mut result = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        result.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        let Some(end) = rest.find('}') else {
            return Err(ConfigError::Invalid(
                "unterminated environment reference".into(),
            ));
        };
        let name = &rest[..end];
        result.push_str(&lookup(name)?);
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}
fn expand_map(map: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    map.iter()
        .map(|(k, v)| Ok((k.clone(), expand(v)?)))
        .collect()
}

/// One consistent version of the loaded configuration.
#[derive(Clone, Default)]
struct Snapshot {
    tools: BTreeMap<String, tools::ToolOverride>,
    config: Config,
    providers: BTreeMap<String, Provider>,
    mcps: BTreeMap<String, Mcp>,
    /// Discovery failures keyed by provider ID. Static models remain available.
    discovery_errors: BTreeMap<String, discovery::DiscoveryError>,
    /// Provider and MCP files that failed to parse or resolve; they are skipped
    /// so one bad file does not hide the others. `config.json` errors stay fatal.
    errors: Vec<String>,
}
type Stamp = BTreeMap<PathBuf, (SystemTime, u64)>;
/// The loaded configuration and the directories it comes from. Reloads are
/// transactional: readers always see one consistent version.
#[derive(Clone)]
pub struct ConfigStore {
    directories: Directories,
    snapshot: Snapshot,
    stamp: Stamp,
}
impl ConfigStore {
    /// Load static configuration and fresh discovery caches without running commands.
    pub fn load(directories: Directories) -> Result<Self> {
        let (mut snapshot, stamp) = load(&directories.config)?;
        discovery::enrich(&mut snapshot, &directories, &BTreeSet::new(), false);
        Ok(Self {
            directories,
            snapshot,
            stamp,
        })
    }
    pub fn refresh_discovery(&mut self, provider: &str) -> Result<()> {
        self.refresh_discovery_many([provider])
    }
    /// Refresh only this provider, respecting success and failure TTLs.
    /// Other command providers never delay a request for this provider.
    pub fn refresh_discovery_provider(&mut self, provider: &str) -> Result<()> {
        let (mut snapshot, stamp) = load(&self.directories.config)?;
        discovery::enrich_selected(
            &mut snapshot,
            &self.directories,
            &BTreeSet::new(),
            true,
            Some(provider),
        );
        self.snapshot = snapshot;
        self.stamp = stamp;
        Ok(())
    }
    /// Reload once and force each selected provider at most once. Failures are
    /// available through [`Self::discovery_error`]; config.json errors stay fatal.
    pub fn refresh_discovery_many<I, S>(&mut self, providers: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let providers = providers
            .into_iter()
            .map(|p| p.as_ref().to_owned())
            .collect();
        let (mut snapshot, stamp) = load(&self.directories.config)?;
        discovery::enrich(&mut snapshot, &self.directories, &providers, true);
        self.snapshot = snapshot;
        self.stamp = stamp;
        Ok(())
    }
    /// Take `provider` and its discovery outcome from `newer`, a store that
    /// just refreshed it; everything else stays as it is.
    pub fn adopt_discovery(&mut self, newer: &ConfigStore, provider: &str) {
        if let Some(next) = newer.snapshot.providers.get(provider) {
            self.snapshot
                .providers
                .insert(provider.to_owned(), next.clone());
        }
        self.snapshot.discovery_errors.remove(provider);
        if let Some(error) = newer.snapshot.discovery_errors.get(provider) {
            self.snapshot
                .discovery_errors
                .insert(provider.to_owned(), error.clone());
        }
    }
    /// Transactional: a failed reload preserves the last valid snapshot. Call at turn boundaries.
    pub fn reload_if_changed(&mut self) -> Result<bool> {
        let unchanged = stamps(&self.directories.config)? == self.stamp;
        if unchanged
            && !self
                .snapshot
                .providers
                .values()
                .any(|p| p.discovery().is_some())
        {
            return Ok(false);
        }
        let (mut snapshot, stamp) = load(&self.directories.config)?;
        discovery::enrich(&mut snapshot, &self.directories, &BTreeSet::new(), false);
        self.snapshot = snapshot;
        self.stamp = stamp;
        Ok(true)
    }
    /// The directories this store loads from (config, data and cache), also
    /// to open another store later.
    pub fn directories(&self) -> Directories {
        self.directories.clone()
    }
    /// The effective `config.json`, with its prompt files resolved.
    pub fn config(&self) -> &Config {
        &self.snapshot.config
    }
    /// The system prompt file: the configured one, else `system.md` in the
    /// config directory (which need not exist).
    pub fn system_prompt_path(&self) -> PathBuf {
        self.snapshot
            .config
            .system_prompt
            .clone()
            .unwrap_or_else(|| self.directories.config.join("system.md"))
    }
    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.snapshot.providers.get(id)
    }
    /// Every loaded provider, enabled or not, by ID.
    pub fn providers(&self) -> impl Iterator<Item = (&String, &Provider)> {
        self.snapshot.providers.iter()
    }
    /// A model by its `provider/model` reference, with its provider.
    pub fn model(&self, id: &str) -> Option<(&Provider, &ModelConfig)> {
        let (provider, model) = id.split_once('/')?;
        let provider = self.provider(provider)?;
        Some((provider, provider.model(model)?))
    }
    /// Every model of every provider: (provider ID, provider, model ID, model).
    pub fn models(&self) -> impl Iterator<Item = (&String, &Provider, &String, &ModelConfig)> {
        self.providers().flat_map(|(provider_id, provider)| {
            provider
                .models()
                .map(move |(model_id, model)| (provider_id, provider, model_id, model))
        })
    }
    pub fn mcp(&self, name: &str) -> Option<&Mcp> {
        self.snapshot.mcps.get(name)
    }
    pub fn mcps(&self) -> impl Iterator<Item = (&String, &Mcp)> {
        self.snapshot.mcps.iter()
    }
    /// The effective tool profile of every model (`provider/model`), of every
    /// provider's other models (`provider/`) and the global one (empty key).
    pub fn tool_profiles(&self) -> BTreeMap<String, tools::Profile> {
        let global = self.snapshot.config.tools.profile;
        let mut profiles = BTreeMap::from([(String::new(), global)]);
        for (pid, provider) in self.providers() {
            profiles.insert(format!("{pid}/"), provider.tool_profile(None, global));
        }
        for (pid, provider, mid, model) in self.models() {
            profiles.insert(
                format!("{pid}/{mid}"),
                provider.tool_profile(Some(model), global),
            );
        }
        profiles
    }
    /// The effective tool overrides for `model` (`provider/model`, `provider/`
    /// or empty): `tools.json`, overlaid by the provider's, then the model's.
    pub fn tool_overrides(&self, model: &str) -> BTreeMap<String, tools::ToolOverride> {
        let mut out = self.snapshot.tools.clone();
        if let Some((p, m)) = model.split_once('/')
            && let Some(provider) = self.provider(p)
        {
            provider.overlay_tool_overrides(m, &mut out);
        }
        out
    }
    /// Skipped provider/MCP files and discovery warnings, for display.
    pub fn errors(&self) -> &[String] {
        &self.snapshot.errors
    }
    /// Why the last discovery of `provider` failed, if it did.
    pub fn discovery_error(&self, provider: &str) -> Option<&discovery::DiscoveryError> {
        self.snapshot.discovery_errors.get(provider)
    }
}
fn file_error(path: &Path, e: impl std::fmt::Display) -> ConfigError {
    ConfigError::File {
        path: path.into(),
        message: e.to_string(),
    }
}
fn stamps(root: &Path) -> Result<Stamp> {
    let mut paths = vec![root.join("config.json")];
    if root.join("config.json").exists() {
        let config: Config = parse(&root.join("config.json"))?;
        paths.extend(config.prompt_files(root));
    }
    paths.push(root.join("tools.json"));
    paths.push(root.join("system.md"));
    for sub in ["providers", "mcps"] {
        let dir = root.join(sub);
        match fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry.map_err(|e| file_error(&dir, e))?.path();
                    if path.extension().is_some_and(|e| e == "json") {
                        paths.push(path);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(file_error(&dir, e)),
        }
    }
    let mut stamp = Stamp::new();
    for path in paths {
        match fs::metadata(&path) {
            Ok(m) => {
                stamp.insert(
                    path.clone(),
                    (m.modified().map_err(|e| file_error(&path, e))?, m.len()),
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(file_error(&path, e)),
        }
    }
    Ok(stamp)
}
fn parse<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).map_err(|e| file_error(path, e))?;
    // Serde errors can include field values. Keep diagnostics to location/category only.
    serde_json::from_slice(&bytes).map_err(|e| {
        file_error(
            path,
            format!(
                "invalid JSON/schema at line {}, column {} ({:?})",
                e.line(),
                e.column(),
                e.classify()
            ),
        )
    })
}
fn load(root: &Path) -> Result<(Snapshot, Stamp)> {
    let stamp = stamps(root)?;
    let mut snapshot = Snapshot::default();
    for path in stamp.keys() {
        if path == &root.join("tools.json") {
            snapshot.tools = parse(path)?;
            continue;
        }
        if path == &root.join("config.json") {
            let config: Config = parse(path)?;
            config.validate().map_err(|e| file_error(path, e))?;
            snapshot.config = config;
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| file_error(path, "invalid filename"))?
            .to_owned();
        if path.parent() == Some(root.join("providers").as_path()) {
            let provider = parse::<Provider>(path).and_then(|mut p| {
                p.resolve().map_err(|e| file_error(path, e))?;
                Ok(p)
            });
            match provider {
                Ok(p) => {
                    snapshot.providers.insert(id, p);
                }
                Err(e) => snapshot.errors.push(e.to_string()),
            }
        } else if path.parent() == Some(root.join("mcps").as_path()) {
            match load_mcp(path) {
                Ok(m) => {
                    snapshot.mcps.insert(id, m);
                }
                Err(e) => snapshot.errors.push(e.to_string()),
            }
        }
    }
    snapshot.config.resolve_prompts(root)?;
    Ok((snapshot, stamp))
}
fn prompt_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    }
}
fn read_prompt(root: &Path, path: &Path) -> Result<(PathBuf, String)> {
    let path = prompt_path(root, path);
    let text = fs::read_to_string(&path).map_err(|e| file_error(&path, e))?;
    if text.trim().is_empty() {
        return Err(file_error(&path, "prompt file is empty"));
    }
    Ok((path, text))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ScriptConfig {
    pub cpu_secs: u64,
    pub max_calls: usize,
}
impl Default for ScriptConfig {
    fn default() -> Self {
        Self {
            cpu_secs: 10,
            max_calls: 1000,
        }
    }
}

/// `generate` keeps provider output in memory up to fixed caps and spills the
/// rest to a private temp file; `maxOutputBytes` is the absolute per-output
/// ceiling (raw response, decoded media or command outputFile).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GenerateConfig {
    pub max_output_bytes: u64,
}
impl Default for GenerateConfig {
    fn default() -> Self {
        Self {
            max_output_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

fn load_mcp(path: &Path) -> Result<Mcp> {
    let mut m: Mcp = parse(path)?;
    m.resolved = Some(ResolvedMcp {
        transport: m.transport.clone(),
        command: m.command.as_deref().map(expand).transpose()?,
        args: m.args.clone(),
        url: m.url.as_deref().map(expand).transpose()?,
        headers: expand_map(&m.headers)?,
        env: expand_map(&m.env)?,
        timeouts: m.timeouts.clone(),
    });
    Ok(m)
}
/// Credentials only come from the environment: `web.apiKey` must be exactly
/// one `${VAR}` reference. The error names the field, never the value.
fn validate_web(web: &WebConfig) -> Result<()> {
    let Some(key) = &web.api_key else {
        return Ok(());
    };
    let reference = key
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|name| {
            name.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    if reference {
        Ok(())
    } else {
        Err(ConfigError::Invalid(
            "web.apiKey must be an environment reference such as \"${FIRECRAWL_API_KEY}\", not a literal key".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_resolution_keeps_noncredential_expansions_visible() {
        // Read an existing variable rather than mutating the process environment
        // while tests and provider processes may run concurrently.
        let value = env::var("PATH").unwrap();
        assert!(value.len() >= 8);
        let mut provider: Provider = serde_json::from_value(serde_json::json!({
            "type":"command",
            "command":{
                "argv":["adapter", "prefix-${PATH}-suffix"],
                "stdin":"input:${PATH}",
                "cwd":"${PATH}",
                "env":{"CUSTOM":"nested:${PATH}"},
                "discover":{"argv":["adapter", "${PATH}"]}
            }
        }))
        .unwrap();
        provider.resolve().unwrap();
        let resolved = provider.connection_command().unwrap();
        assert!(resolved.redaction_values.is_empty());
        assert_eq!(resolved.argv[1], format!("prefix-{value}-suffix"));
        assert_eq!(
            resolved.stdin.as_deref(),
            Some(format!("input:{value}").as_str())
        );
        assert_eq!(resolved.env["CUSTOM"], format!("nested:{value}"));
        // Expanded credentials stay out of serialized provider configuration.
        assert!(!serde_json::to_string(&provider).unwrap().contains(&value));
    }

    #[test]
    fn credential_names_match_whole_tokens_and_exclude_auth_socket() {
        for name in [
            "KEY",
            "APIKEY",
            "TOKEN",
            "SECRET",
            "PASSWORD",
            "PASSWD",
            "CREDENTIAL",
            "CREDENTIALS",
            "AUTH",
            "OPENAI_API_KEY",
            "GITHUB_TOKEN",
            "AUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "client_SeCrEt",
            "service_credentials_path",
            "GITHUBTOKEN",
            "ACCESSTOKEN",
            "DBPASSWORD",
            "PGPASSWORD",
            "GH_PAT",
            "DB_PASS",
            "MYSQL_PWD",
        ] {
            assert!(is_credential_name(name), "{name}");
        }
        for name in [
            "HOME",
            "PATH",
            "GIT_AUTHOR_NAME",
            "SSH_AUTH_SOCK",
            "ssh_auth_sock",
            "XAUTHORITY",
            "AUTHORIZATION",
            "MONKEY",
            "TOKENIZER",
            "SECRETARY",
            "PWD",
            "OLDPWD",
            "PATH_INFO",
        ] {
            assert!(!is_credential_name(name), "{name}");
        }
    }

    #[test]
    fn command_expansions_collect_only_credential_values_of_at_least_eight_bytes() {
        let mut secrets = Vec::new();
        let expanded = expand_command_with(
            "${HOME}/bin/adapter ${OPENAI_API_KEY} ${APIKEY} ${SHORT_TOKEN} ${SSH_AUTH_SOCK} ${GIT_AUTHOR_NAME} ${XAUTHORITY}",
            &mut secrets,
            |name| Ok(match name {
                "HOME" => "/home/alice",
                "OPENAI_API_KEY" => "expanded-secret",
                "APIKEY" => "12345678",
                "SHORT_TOKEN" => "1234567",
                "SSH_AUTH_SOCK" => "/tmp/auth-socket",
                "GIT_AUTHOR_NAME" => "Alice Example",
                "XAUTHORITY" => "/home/alice/.Xauthority",
                _ => panic!("unexpected variable"),
            }.into()),
        ).unwrap();
        assert!(expanded.starts_with("/home/alice/bin/adapter expanded-secret"));
        assert_eq!(secrets, ["expanded-secret", "12345678"]);
    }

    #[test]
    fn environment_expansion_keeps_substituted_values_opaque() {
        let result = expand_with("prefix-${VALUE}-${SHORT}", |name| {
            Ok(match name {
                "VALUE" => "secret-${NESTED}",
                "SHORT" => "tiny",
                _ => panic!("expanded a substituted value"),
            }
            .into())
        })
        .unwrap();
        assert_eq!(result, "prefix-secret-${NESTED}-tiny");
        assert!(expand_with("${OPEN", |_| panic!("unexpected lookup")).is_err());
    }

    #[test]
    fn subagents_round_trip_as_an_object_with_default_limits() {
        let value = serde_json::to_value(Subagents::default()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"mode":"tree", "maxDepth":null,
            "maxConcurrentChildren":4, "maxTotalAgents":null})
        );
        let parsed: Subagents = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.mode, SubagentMode::Tree);
        assert_eq!(parsed.max_concurrent_children, 4);
        assert!(serde_json::from_str::<Subagents>(r#""tree""#).is_err());
        assert!(serde_json::from_str::<Subagents>(r#""flat""#).is_err());
    }

    #[test]
    fn subagents_object_preserves_explicit_limits() {
        let parsed: Subagents = serde_json::from_value(serde_json::json!({
            "mode":"flat", "maxDepth":1, "maxConcurrentChildren":2, "maxTotalAgents":5
        }))
        .unwrap();
        assert_eq!(parsed.mode, SubagentMode::Flat);
        assert_eq!(parsed.max_depth, Some(1));
        assert_eq!(parsed.max_concurrent_children, 2);
        assert_eq!(parsed.max_total_agents, Some(5));
    }
}
