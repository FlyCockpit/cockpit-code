//! Session-only policy and no-history model safety gate, lifted from Cockpit with DB/redaction removed.
pub mod classify;
use crate::{
    Control,
    model::Model,
    schema::*,
    tools::{PermissionGate, PreparedOperation},
};
use anyhow::{Result, bail};
use async_trait::async_trait;
use code_config::{PermissionMode, Snapshot, ThinkingMode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(crate) fn session_gate(
    control: Arc<Control>,
    config: Arc<Mutex<Snapshot>>,
    custom: Option<Arc<dyn PermissionGate>>,
) -> Arc<dyn PermissionGate> {
    let policy = SessionGate::new(control, config);
    if let Some(custom) = custom {
        Arc::new(CustomGate { policy, custom })
    } else {
        Arc::new(policy)
    }
}
struct CustomGate {
    policy: SessionGate,
    custom: Arc<dyn PermissionGate>,
}
#[async_trait]
impl PermissionGate for CustomGate {
    async fn revise(
        &self,
        operation: &PreparedOperation,
        improve: bool,
        cancel: &CancellationToken,
    ) -> Result<Option<(ToolCall, String)>> {
        if self.policy.output_policy(operation)?.is_some() {
            return Ok(None);
        }
        if improve {
            self.custom.revise(operation, true, cancel).await
        } else {
            self.policy.revise(operation, false, cancel).await
        }
    }
    async fn authorize(
        &self,
        operation: &PreparedOperation,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if self.policy.output_policy(operation)?.is_some() {
            return Ok(());
        }
        if crate::guidance::protected_write(operation) {
            self.policy
                .ask(
                    operation,
                    "Agent changes to reviewer guidance require approval in every mode.".into(),
                    None,
                    cancel,
                )
                .await?;
        }
        self.custom.authorize(operation, cancel).await
    }
    fn commit(&self, action: Box<dyn FnOnce() -> Result<()> + Send + '_>) -> Result<()> {
        self.custom.commit(action)
    }
}

pub(crate) struct SessionGate {
    pub control: Arc<Control>,
    pub config: Arc<Mutex<Snapshot>>,
    grants: Mutex<BTreeSet<String>>,
    approval_queue: tokio::sync::Mutex<()>,
    libraries: tokio::sync::Mutex<crate::reviewer::Libraries>,
    prechecks: Mutex<HashMap<String, (String, SafetyVerdict)>>,
    final_checks: Mutex<BTreeSet<String>>,
}
impl SessionGate {
    /// Some means a proven artifact read. An artifact write always fails before mode/grants.
    pub(crate) fn output_policy(&self, op: &PreparedOperation) -> Result<Option<()>> {
        let dir = self.control.outputs_dir();
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        if op
            .target
            .as_ref()
            .is_some_and(|path| path.starts_with(&dir))
        {
            if matches!(op.call.name.as_str(), "read" | "grep" | "glob") {
                return Ok(Some(()));
            }
            if op.call.name != "bash" {
                bail!("session output files are read-only; writes there are refused");
            }
        }
        if let Some(command) = &op.command {
            let resolver = crate::paths::Resolver::new(op.effective_cwd().to_path_buf());
            let mut mentions = op
                .execution_cwd
                .as_ref()
                .is_some_and(|cwd| cwd.starts_with(&dir))
                || command.contains(dir.to_string_lossy().as_ref());
            if let classify::Classification::Parsed {
                simple_commands, ..
            } = classify::classify(command)
            {
                mentions |= simple_commands
                    .iter()
                    .flat_map(|c| c.args.iter().chain(c.risk.affected_paths.iter()))
                    .chain(crate::subscriptions::shell_write_paths(command).iter())
                    .any(|raw| resolver.resolve(raw).is_ok_and(|p| p.starts_with(&dir)));
            }
            if mentions {
                if static_read_only_in(command, op.effective_cwd(), Some(&dir)) {
                    return Ok(Some(()));
                }
                bail!(
                    "session output files are read-only; use read/grep or a simple read-only shell command"
                );
            }
        }
        Ok(None)
    }
    pub fn new(control: Arc<Control>, config: Arc<Mutex<Snapshot>>) -> Self {
        Self {
            control,
            config,
            grants: Mutex::default(),
            approval_queue: tokio::sync::Mutex::new(()),
            libraries: tokio::sync::Mutex::new(crate::reviewer::Libraries::default()),
            prechecks: Mutex::default(),
            final_checks: Mutex::default(),
        }
    }
}
fn command_key(operation: &PreparedOperation) -> Option<String> {
    let command = operation.command.as_deref()?;
    let classify::Classification::Parsed {
        simple_commands,
        compound: false,
    } = classify::classify(command)
    else {
        return None;
    };
    if simple_commands.len() != 1 {
        return None;
    }
    let c = &simple_commands[0];
    if c.wrapper || c.execution_bearing_option {
        return None;
    }
    Some(format!(
        "bash:{}:{}",
        c.normalized_program,
        c.subcommand.as_deref().unwrap_or("")
    ))
}
fn path_grant(operation: &PreparedOperation) -> Option<(String, PathBuf)> {
    let path = operation.target.as_ref()?;
    let prefix = if path.is_dir() {
        path.clone()
    } else {
        path.parent()?.to_owned()
    };
    Some((operation.call.name.clone(), prefix))
}
/// Web session grants: `web_search` as a whole, `web_fetch` per host.
fn web_key(operation: &PreparedOperation) -> Option<String> {
    match operation.call.name.as_str() {
        "web_search" => Some("web:search".into()),
        "web_fetch" | "read_image" | "read_video" => {
            reqwest::Url::parse(operation.call.arguments["url"].as_str()?)
                .ok()?
                .host_str()
                .map(|host| format!("web:fetch:{host}"))
        }
        _ => None,
    }
}
fn web_tool(operation: &PreparedOperation) -> bool {
    matches!(operation.call.name.as_str(), "web_search" | "web_fetch")
        || (matches!(operation.call.name.as_str(), "read_image" | "read_video")
            && operation.call.arguments["url"].is_string())
}
fn grant_key(operation: &PreparedOperation) -> Option<String> {
    if web_tool(operation) {
        return web_key(operation);
    }
    if operation.call.name == "mcp" {
        return Some(format!(
            "mcp:{}:{}",
            operation.call.arguments["server"].as_str()?,
            operation.call.arguments["tool"].as_str()?
        ));
    }
    command_key(operation).or_else(|| {
        path_grant(operation).map(|(tool, prefix)| format!("path:{tool}:{}", prefix.display()))
    })
}
fn granted(grants: &BTreeSet<String>, operation: &PreparedOperation) -> bool {
    if web_tool(operation) || operation.call.name == "mcp" {
        return grant_key(operation).is_some_and(|key| grants.contains(&key));
    }
    if let Some(key) = command_key(operation) {
        return grants.contains(&key);
    }
    operation.target.as_ref().is_some_and(|path| {
        grants.iter().any(|key| {
            key.strip_prefix(&format!("path:{}:", operation.call.name))
                .is_some_and(|prefix| path.starts_with(Path::new(prefix)))
        })
    })
}
/// Conservative allowlist. Complex syntax, substitutions, wrappers and unknown flags use the sidecar.
pub fn static_read_only(command: &str, cwd: &Path) -> bool {
    static_read_only_in(command, cwd, None)
}
fn static_read_only_in(command: &str, cwd: &Path, outputs: Option<&Path>) -> bool {
    let classify::Classification::Parsed {
        simple_commands,
        compound: false,
    } = classify::classify(command)
    else {
        return false;
    };
    if simple_commands.len() != 1 {
        return false;
    }
    let resolver = crate::paths::Resolver::new(cwd.into());
    let c = &simple_commands[0];
    if c.wrapper || c.execution_bearing_option || c.normalized_program.contains('/') {
        return false;
    }
    let (allowed, skip) = match c.normalized_program.as_str() {
        "pwd" => (true, 0),
        "ls" | "cat" | "head" | "tail" | "wc" | "rg" | "grep" => (true, 0),
        "git" => (
            c.args.first().is_some_and(|s| {
                ["status", "diff", "log", "show", "rev-parse", "ls-files"].contains(&s.as_str())
            }),
            1,
        ),
        _ => (false, 0),
    };
    if !allowed {
        return false;
    }
    for raw in c.args.iter().skip(skip) {
        if raw.contains([
            '$', '`', '*', '?', '[', ']', '{', '}', '\\', '<', '>', '(', ')',
        ]) {
            return false;
        }
        let raw = raw
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(raw);
        if raw.starts_with('-') {
            if ![
                "--",
                "-l",
                "-a",
                "-la",
                "-al",
                "-n",
                "-h",
                "-i",
                "-s",
                "-S",
                "-F",
                "-c",
                "-E",
                "-v",
                "--count",
                "--line-number",
                "--short",
                "--stat",
                "--name-only",
                "--cached",
                "--staged",
                "--oneline",
                "--no-ext-diff",
                "--no-textconv",
            ]
            .contains(&raw)
            {
                return false;
            }
        } else if !resolver.resolve(raw).is_ok_and(|p| {
            (p.starts_with(cwd) || outputs.is_some_and(|dir| p.starts_with(dir)))
                && !credential_path(&p)
        }) {
            return false;
        }
    }
    true
}
fn credential_path(path: &Path) -> bool {
    let parts = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>();
    parts.iter().any(|p| {
        [
            ".ssh",
            ".aws",
            "keychains",
            ".mozilla",
            "firefox",
            "chromium",
            "chrome",
        ]
        .contains(&p.as_str())
            || p.starts_with(".env")
            || p == "id_ed25519"
            || p == "id_rsa"
            || p.ends_with(".pem")
    }) || parts.windows(2).any(|p| p[0] == ".config" && p[1] == "gh")
}
pub const TOOL_CALL_PROMPT: &str = include_str!("../builtin/safety-tool-call.md");
pub const WRITE_EDIT_PROMPT: &str = include_str!("../builtin/safety-write-edit.md");

/// Validate the session safety authority independently of subagent eligibility.
pub(crate) fn validate_model(snapshot: &Snapshot, id: &str) -> Result<(), EngineError> {
    let (provider, model) = id
        .split_once('/')
        .ok_or_else(|| EngineError::bad("safety model must be provider/model"))?;
    let entry = snapshot
        .providers
        .get(provider)
        .and_then(|provider| provider.models.get(model))
        .ok_or_else(|| EngineError::bad(format!("safety model {id} is not configured")))?;
    if !entry
        .capabilities
        .input
        .contains(&code_config::Modality::Text)
        || !entry
            .capabilities
            .output
            .contains(&code_config::Modality::Text)
    {
        return Err(EngineError::bad(
            "safety model must support text input and text output",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Verdict {
    Safe,
    Unsafe,
    Improve,
}
impl Verdict {
    fn name(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Unsafe => "unsafe",
            Self::Improve => "improve",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SafetyVerdict {
    #[serde(default)]
    focuses: Vec<String>,
    verdict: Verdict,
    reason: String,
    hint: String,
}
fn operation_key(op: &PreparedOperation) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(serde_json::to_vec(op).unwrap()))
}
fn needs_review(op: &PreparedOperation) -> bool {
    matches!(
        op.call.name.as_str(),
        "bash" | "mcp" | "write" | "edit" | "web_search" | "web_fetch"
    ) || (op.call.name == "jobs" && op.call.arguments["action"] == "input")
        || op.outside()
        || web_tool(op)
}
impl SessionGate {
    async fn sidecar(
        &self,
        op: &PreparedOperation,
        cancel: &CancellationToken,
    ) -> Result<SafetyVerdict> {
        let id = crate::exclusive::safety(&self.control.journal.lock().unwrap().settings);
        let model_id = id.as_deref().unwrap_or("");
        let review = |phase: &str, verdict: Option<String>, reason: Option<String>| {
            self.control.emit_for(
                &op.agent_id,
                EngineEvent::SafetyReview {
                    call_id: op.call.id.clone(),
                    agent_id: op.agent_id.clone(),
                    breadcrumb: op.breadcrumb.clone(),
                    model: model_id.into(),
                    phase: phase.into(),
                    verdict,
                    reason,
                },
            );
        };
        review("started", None, None);
        let safety_only = self.final_checks.lock().unwrap().contains(&op.call.id);
        let result = self
            .sidecar_request(op, cancel, id.as_deref(), safety_only)
            .await;
        review(
            "finished",
            result.as_ref().ok().map(|v| v.verdict.name().into()),
            Some(match &result {
                Ok(v) => v.reason.clone(),
                Err(e) => e.to_string(),
            }),
        );
        result
    }
    async fn sidecar_request(
        &self,
        op: &PreparedOperation,
        cancel: &CancellationToken,
        id: Option<&str>,
        safety_only: bool,
    ) -> Result<SafetyVerdict> {
        let snapshot = self.config.lock().unwrap().clone();
        let id = id.ok_or_else(|| anyhow::anyhow!("safety model unavailable"))?;
        validate_model(&snapshot, id)?;
        let (provider, model) = id
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("safety model unavailable"))?;
        let provider_config = snapshot
            .providers
            .get(provider)
            .ok_or_else(|| anyhow::anyhow!("safety model unavailable"))?;
        let mut config = provider_config
            .models
            .get(model)
            .ok_or_else(|| anyhow::anyhow!("safety model unavailable"))?
            .clone();
        config.reasoning.default = None;
        config.reasoning.efforts.clear();
        config.thinking = Some(ThinkingMode::None);
        config.max_output_tokens = Some(2048);
        let model = Model::new(
            provider.into(),
            model.into(),
            provider_config.clone(),
            config,
            None,
        )
        .map_err(|_| anyhow::anyhow!("safety model unavailable"))?;
        let context = {
            let j = self.control.journal.lock().unwrap();
            let history = j.histories.get(&op.agent_id);
            let latest = history.and_then(|h| {
                h.iter().rev().find_map(|e| {
                    if let HistoryEvent::User { text, .. } | HistoryEvent::Message { text, .. } = e
                    {
                        Some(text)
                    } else {
                        None
                    }
                })
            });
            json!({"action":op,"contract":j.contracts.get(&op.agent_id).map(crate::goal::GoalContract::open_context),"latestMessage":latest})
        };
        let prompts = &snapshot.config.permissions.prompts;
        let system = if matches!(op.call.name.as_str(), "write" | "edit") {
            prompts
                .resolved_write_edit
                .as_deref()
                .unwrap_or(WRITE_EDIT_PROMPT)
        } else {
            prompts
                .resolved_tool_call
                .as_deref()
                .unwrap_or(TOOL_CALL_PROMPT)
        };
        let system = format!(
            "{system}\n\nThe improve verdict may include an optional focuses array of short strings, one perspective per variation. Return the same verdict/reason/hint fields plus focuses when useful."
        );
        let project = self
            .control
            .project_reviewer_guidance(&op.cwd, cancel)
            .await?;
        let mut system = system;
        if let Some(global) = &snapshot.config.permissions.resolved_reviewer_guidance {
            system.push_str(&format!("\n\nGlobal reviewer guidance:\n{global}"));
        }
        if let Some(project) = project {
            system.push_str(&format!(
                "\n\nTrusted project reviewer guidance:\n{project}"
            ));
        }
        let mut libraries = self.libraries.lock().await;
        let verdict: SafetyVerdict = serde_json::from_value(
            crate::reviewer::check(
                &mut libraries,
                &self.control,
                &model,
                &op.cwd,
                &system,
                context,
                &op.agent_id,
                safety_only,
                cancel,
            )
            .await?,
        )
        .map_err(|_| anyhow::anyhow!("reviewer returned invalid verdict"))?;
        let approval_id = uuid::Uuid::new_v4().to_string();
        self.control.commit(
            &op.agent_id,
            HistoryEvent::Approval {
                id: approval_id.clone(),
                granted: matches!(verdict.verdict, Verdict::Safe | Verdict::Improve),
                reason: verdict.reason.clone(),
                verdict: Some(
                    match verdict.verdict {
                        Verdict::Safe => "safe",
                        Verdict::Unsafe => "unsafe",
                        Verdict::Improve => "improve",
                    }
                    .into(),
                ),
                hint: Some(verdict.hint.clone()),
            },
        )?;
        let choice = if matches!(verdict.verdict, Verdict::Safe | Verdict::Improve) {
            "approve"
        } else {
            "reject"
        };
        let mut record = crate::DecisionRecord::new(
            "safety",
            &op.agent_id,
            format!(
                "Allow {}{}?",
                op.call.name,
                op.target
                    .as_ref()
                    .map(|p| format!(" {}", p.display()))
                    .unwrap_or_default()
            ),
            choice.into(),
            vec![
                if choice == "approve" {
                    "reject"
                } else {
                    "approve"
                }
                .into(),
            ],
            verdict.reason.clone(),
        );
        record.id = format!("safety-{approval_id}");
        record.affected_files = op
            .target
            .iter()
            .cloned()
            .chain(op.edits.iter().map(|e| e.path.clone()))
            .collect();
        record.affected_files.sort();
        record.affected_files.dedup();
        self.control.record_decision(record)?;
        Ok(verdict)
    }
    async fn ask(
        &self,
        op: &PreparedOperation,
        reason: String,
        sidecar: Option<SafetyVerdict>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let _queue = tokio::select! {
            _ = cancel.cancelled() => bail!("cancelled while awaiting approval"),
            guard = self.approval_queue.lock() => guard,
        };
        let force_once = crate::guidance::protected_write(op);
        if !force_once && granted(&self.grants.lock().unwrap(), op) {
            return Ok(());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.control
            .approvals
            .lock()
            .unwrap()
            .insert(id.clone(), tx);
        let request = ApprovalRequest {
            call_id: op.call.id.clone(),
            parent_call_id: op.parent_call_id.clone(),
            id: id.clone(),
            agent_id: op.agent_id.clone(),
            breadcrumb: op.breadcrumb.clone(),
            job_id: op.job_id.clone(),
            tool: op.call.name.clone(),
            target: op.target.as_ref().map(|p| p.display().to_string()),
            diff: op.diff.clone(),
            command: op.command.clone(),
            requested: op.requested.arguments.clone(),
            resolved: op.call.arguments.clone(),
            reason,
            sidecar_verdict: sidecar.as_ref().map(|s| {
                match s.verdict {
                    Verdict::Safe => "safe",
                    Verdict::Unsafe => "unsafe",
                    Verdict::Improve => "improve",
                }
                .into()
            }),
            sidecar_reason: sidecar.as_ref().map(|s| s.reason.clone()),
            sidecar_hint: sidecar.as_ref().map(|s| s.hint.clone()),
            grant_key: if force_once { None } else { grant_key(op) },
        };
        self.control.commit(
            &op.agent_id,
            HistoryEvent::ApprovalRequested {
                request: request.clone(),
            },
        )?;
        self.control
            .goal_clock
            .lock()
            .unwrap()
            .wait(format!("approval:{id}"));
        self.control.emit(EngineEvent::ApprovalRequest { request });
        let decision = tokio::select! {_ = cancel.cancelled()=>{self.control.approvals.lock().unwrap().remove(&id);self.control.goal_clock.lock().unwrap().resume(&format!("approval:{id}"));bail!("cancelled while awaiting approval");},r=rx=>match r {Ok(decision)=>decision,Err(_)=>{self.control.goal_clock.lock().unwrap().resume(&format!("approval:{id}"));bail!("approval channel closed");}}};
        self.control
            .goal_clock
            .lock()
            .unwrap()
            .resume(&format!("approval:{id}"));
        let (allowed, feedback) = match decision {
            ApprovalDecision::Once => (true, "allowed once".to_owned()),
            ApprovalDecision::Session => {
                if let Some(key) = grant_key(op).filter(|_| !force_once) {
                    self.grants.lock().unwrap().insert(key.clone());
                    (true, format!("allowed for session: {key}"))
                } else {
                    (
                        true,
                        "allowed once; wrappers/compound operations cannot receive a session grant"
                            .into(),
                    )
                }
            }
            ApprovalDecision::Deny { feedback } => (false, feedback),
        };
        self.control.commit(
            &op.agent_id,
            HistoryEvent::Approval {
                id,
                granted: allowed,
                reason: feedback.clone(),
                verdict: None,
                hint: None,
            },
        )?;
        if !allowed {
            bail!("permission denied: {feedback}");
        }
        if force_once {
            self.control
                .approve_guidance_write(op)
                .map_err(|e| anyhow::anyhow!(e.message))?;
        }
        Ok(())
    }
}
#[async_trait]
impl PermissionGate for SessionGate {
    async fn revise(
        &self,
        op: &PreparedOperation,
        improve: bool,
        cancel: &CancellationToken,
    ) -> Result<Option<(ToolCall, String)>> {
        self.control.check_owner()?;
        if self.output_policy(op)?.is_some() {
            return Ok(None);
        }
        let snapshot = self.config.lock().unwrap().clone();
        if !improve {
            if !matches!(op.call.name.as_str(), "write" | "edit") {
                return Ok(None);
            }
            let count = self
                .control
                .journal
                .lock()
                .unwrap()
                .requests
                .get(&op.agent_id)
                .map(|r| r.config.self_evaluate_writes)
                .unwrap_or(1);
            return match crate::variations::select(
                &self.control,
                &snapshot,
                op,
                count,
                "",
                &[],
                "self_eval",
                cancel,
            )
            .await
            {
                Ok(selected) => Ok(selected),
                Err(_) if cancel.is_cancelled() => bail!("cancelled"),
                Err(_) => Ok(None),
            };
        }
        let mode = self
            .control
            .journal
            .lock()
            .unwrap()
            .settings
            .mode
            .unwrap_or(snapshot.config.permissions.mode);
        if !matches!(mode, PermissionMode::Auto | PermissionMode::Unattended)
            || !needs_review(op)
            || granted(&self.grants.lock().unwrap(), op)
            || (!op.outside()
                && !web_tool(op)
                && op
                    .command
                    .as_ref()
                    .is_some_and(|c| static_read_only(c, op.effective_cwd())))
        {
            return Ok(None);
        }
        let verdict = match self.sidecar(op, cancel).await {
            Ok(verdict) => verdict,
            Err(_) if cancel.is_cancelled() => bail!("cancelled"),
            Err(_) => return Ok(None),
        };
        if matches!(verdict.verdict, Verdict::Improve) {
            self.final_checks.lock().unwrap().insert(op.call.id.clone());
            let selected = crate::variations::select(
                &self.control,
                &snapshot,
                op,
                snapshot.config.permissions.auto_variations,
                &verdict.hint,
                &verdict.focuses,
                "improve",
                cancel,
            )
            .await;
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            return Ok(Some(selected.ok().flatten().unwrap_or_else(||(op.call.clone(),"improvement selection unavailable; retained original for final safety-only check".into()))));
        }
        let mut prechecks = self.prechecks.lock().unwrap();
        if prechecks.len() >= 256 {
            prechecks.clear();
        }
        prechecks.insert(op.call.id.clone(), (operation_key(op), verdict));
        Ok(None)
    }
    fn commit(&self, action: Box<dyn FnOnce() -> Result<()> + Send + '_>) -> Result<()> {
        self.control.journal.lock().unwrap().with_owner(action)
    }
    async fn authorize(&self, op: &PreparedOperation, cancel: &CancellationToken) -> Result<()> {
        self.control.check_owner()?;
        if self.output_policy(op)?.is_some() {
            return Ok(());
        }
        self.control.commit(
            &op.agent_id,
            HistoryEvent::OperationPrepared {
                operation: serde_json::to_value(op)?,
            },
        )?;
        let override_mode = self.control.journal.lock().unwrap().settings.mode;
        let mode =
            override_mode.unwrap_or_else(|| self.config.lock().unwrap().config.permissions.mode);
        crate::exclusive::check_mode(&self.control.journal.lock().unwrap().settings, mode)?;
        let final_check = self.final_checks.lock().unwrap().contains(&op.call.id);
        let gated = final_check
            || op.call.name == "bash"
            || (op.call.name == "jobs" && op.call.arguments["action"] == "input")
            || op.outside()
            || op.call.name == "mcp"
            || web_tool(op)
            || (matches!(mode, PermissionMode::Auto | PermissionMode::Unattended)
                && (matches!(op.call.name.as_str(), "write" | "edit")
                    || (op.call.name == "generate" && op.target.is_some())));
        if crate::guidance::protected_write(op) {
            return self
                .ask(
                    op,
                    "Agent changes to reviewer guidance require approval in every mode.".into(),
                    None,
                    cancel,
                )
                .await;
        }
        if !gated || mode == PermissionMode::Yolo {
            return Ok(());
        }
        if !final_check && granted(&self.grants.lock().unwrap(), op) {
            return Ok(());
        }
        if mode == PermissionMode::Manual {
            return self
                .ask(op, "manual permission mode".into(), None, cancel)
                .await;
        }
        if !final_check
            && !op.outside()
            && !web_tool(op)
            && op
                .command
                .as_ref()
                .is_some_and(|command| static_read_only(command, op.effective_cwd()))
        {
            return Ok(());
        }
        let cached = self
            .prechecks
            .lock()
            .unwrap()
            .remove(&op.call.id)
            .filter(|(key, _)| key == &operation_key(op))
            .map(|(_, v)| v);
        let verdict = if let Some(verdict) = cached {
            Ok(verdict)
        } else {
            self.sidecar(op, cancel).await
        };
        self.final_checks.lock().unwrap().remove(&op.call.id);
        match verdict {
            Ok(verdict) if matches!(verdict.verdict, Verdict::Safe | Verdict::Improve) => Ok(()),
            Ok(verdict) if mode == PermissionMode::Unattended => {
                bail!("permission denied: {}", verdict.reason)
            }
            Err(_) if mode == PermissionMode::Unattended => bail!("safety model unavailable"),
            Ok(verdict) => {
                self.ask(op, verdict.reason.clone(), Some(verdict), cancel)
                    .await
            }
            Err(_) => {
                self.ask(op, "safety model unavailable".into(), None, cancel)
                    .await
            }
        }
    }
}
