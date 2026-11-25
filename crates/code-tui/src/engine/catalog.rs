//! The configured models and providers as the UI sees them: the `/model`
//! catalog, which providers need discovery, and each provider's usage.
use super::views::QueueClass;
use crate::usage::{self, Budget, ProviderUsage};
use code_config::{Billing, ConfigStore, Directories, Modality, ModelToggles};
use serde_json::Value;
use std::path::{Path, PathBuf};

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
    /// `queueDefault`.
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

/// One provider's usage from the engine's shared cache:
/// the `cockpit-code.usage/1` document, budget spend over the period from
/// the harness's cost records, and stale/refresh state. A provider with no
/// usage document falls back to its discovery `account` quota.
pub(super) fn usage_of(
    dirs: &Directories,
    p: &code_config::Provider,
    u: code_engine::usage::ProviderUsage,
) -> ProviderUsage {
    let subscription = p.is_subscription();
    let mut out = ProviderUsage {
        label: u
            .label
            .clone()
            .or_else(|| p.label.clone())
            .unwrap_or_else(|| u.provider.clone()),
        provider: u.provider.clone(),
        subscription,
        declared: p.usage_ttl_secs().is_some(),
        budget: p.budget().map(|b| Budget {
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

/// The catalog of `snap`; under `/exclusive` only its `exclusive` model.
pub(super) fn build(snap: &ConfigStore, exclusive: Option<&str>, config_dir: &Path) -> Catalog {
    let mut models = Vec::new();
    let toggles = ModelToggles::default();
    for (pid, provider, mid, m) in snap.models() {
        if exclusive.is_some_and(|ex| ex != format!("{pid}/{mid}")) {
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
            subagent_capable: code_config::subagent_eligible(pid, mid, provider, m, &toggles),
            chat_capable: text_only || m.capabilities.output.contains(&Modality::Text),
            subscription: matches!(m.billing, Billing::Subscription),
            context_unknown: provider.latest_history_only(m),
        });
    }
    let mut benchmarks: Vec<Benchmark> = snap
        .config()
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
    // Discovery failures are structured per provider; model
    // warnings ("{id}/{model}: Warning: …") are in the model description
    // already. Anything else is a skipped file.
    let providers: Vec<ProviderView> = snap
        .providers()
        .map(|(id, p)| {
            let failure = snap.discovery_error(id);
            ProviderView {
                id: id.clone(),
                label: p.label.clone().unwrap_or_else(|| id.clone()),
                enabled: p.enabled,
                discovers: p.discovery().is_some(),
                warning: failure.map(|e| e.message.clone()),
                warning_detail: failure.map(|e| {
                    let kind = serde_json::to_value(e.kind)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    let mut parts = vec![kind];
                    if e.cached {
                        parts.push("cached".into());
                        if let Some(t) = chrono::DateTime::from_timestamp(e.retry_at as i64, 0) {
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
    for e in snap.errors() {
        let (head, rest) = e.split_once(": ").unwrap_or(("", e));
        let model_warning = snap.model(head).is_some() && rest.starts_with("Warning:");
        let discovery = snap.discovery_error(head).is_some()
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
        theme: snap.config().theme.clone(),
        mouse: snap.config().mouse,
        attention: snap.config().attention.clone(),
        vim: snap.config().vim,
        paste_collapse_lines: snap.config().paste_collapse_lines,
        queue_default: QueueClass::from_config(snap.config().queue_default),
        config_dir: config_dir.to_path_buf(),
        config_var,
        errors,
    }
}
