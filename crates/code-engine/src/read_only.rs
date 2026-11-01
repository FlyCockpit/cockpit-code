//! A closed read-only host used by reviewers and hidden variations.
use crate::{
    schema::{EngineError, ToolCall},
    script::{HostFunction, MontyRuntime, ScriptLimits, ScriptProgress, ScriptRuntime},
    tools::{CommitBarrier, PermissionGate, PreparedOperation, ToolBox, ToolDefinition},
};
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

pub(crate) const ALLOWED: &str =
    "Allowed actions: read, grep, glob, and script using only read, grep, glob.";
#[derive(Clone)]
pub(crate) struct Receipt {
    pub path: PathBuf,
    pub hash: String,
    pub args: Value,
    pub output: String,
}
impl Receipt {
    pub fn unchanged(&self) -> bool {
        digest(&self.path).as_deref() == Some(&self.hash)
    }
}
fn digest(path: &std::path::Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
        return None;
    }
    std::fs::read(path)
        .ok()
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
}
struct Scope(Vec<PathBuf>);
#[async_trait]
impl PermissionGate for Scope {
    async fn authorize(&self, op: &PreparedOperation, _: &CancellationToken) -> Result<()> {
        if !matches!(op.call.name.as_str(), "read" | "grep" | "glob") {
            bail!("{ALLOWED}");
        }
        if op
            .target
            .as_ref()
            .is_none_or(|path| !self.0.iter().any(|root| path.starts_with(root)))
        {
            bail!("Read-only scope is limited to the session cwd and git root. {ALLOWED}");
        }
        Ok(())
    }
}
/// D64 fork chats may use the web tools; they pass through the session's policy.
#[derive(Clone)]
pub(crate) struct WebAccess {
    pub gate: Arc<dyn PermissionGate>,
    pub config: Arc<Mutex<code_config::Snapshot>>,
    pub blocked: Arc<dyn Fn() -> bool + Send + Sync>,
    pub agent_id: String,
    pub breadcrumb: String,
}
#[derive(Clone)]
pub(crate) struct ReadOnly {
    allowed: Option<std::collections::BTreeSet<String>>,
    aliases: crate::tool_names::Names,
    checks: Option<CheckAccess>,
    web: Option<WebAccess>,
    tools: ToolBox,
    resolver: crate::paths::Resolver,
    roots: Vec<PathBuf>,
    pub receipts: Arc<Mutex<Vec<Receipt>>>,
}
impl ReadOnly {
    pub(crate) fn with_outputs(mut self, dir: PathBuf) -> Self {
        self.roots.push(dir);
        self.tools = ToolBox::new(
            self.resolver.cwd.clone(),
            Arc::new(Scope(self.roots.clone())),
        );
        self
    }
    pub(crate) fn with_allowed(mut self, allowed: std::collections::BTreeSet<String>) -> Self {
        self.allowed = Some(allowed);
        self
    }
    fn available(&self, name: &str) -> bool {
        self.allowed.as_ref().is_none_or(|a| a.contains(name))
    }
    pub(crate) fn with_aliases(
        mut self,
        names: std::collections::BTreeMap<String, String>,
    ) -> Self {
        self.aliases = crate::tool_names::Names { names };
        self
    }
    pub fn new(cwd: PathBuf) -> Self {
        let mut roots = vec![cwd.clone()];
        if let Some(root) = cwd.ancestors().find(|p| p.join(".git").exists()) {
            roots.push(root.to_owned());
        }
        Self {
            allowed: None,
            aliases: crate::tool_names::Names::default(),
            checks: None,
            web: None,
            tools: ToolBox::new(cwd.clone(), Arc::new(Scope(roots.clone()))),
            resolver: crate::paths::Resolver::new(cwd),
            roots,
            receipts: Arc::default(),
        }
    }
    pub(crate) fn with_checks(mut self, checks: CheckAccess) -> Self {
        self.checks = Some(checks);
        self
    }
    pub fn definitions() -> Vec<ToolDefinition> {
        let mut defs = ToolBox::definitions()
            .into_iter()
            .filter(|t| matches!(t.name.as_str(), "read" | "grep" | "glob"))
            .collect::<Vec<_>>();
        defs.push(crate::script::definition());
        defs
    }
    pub fn with_web(mut self, web: WebAccess) -> Self {
        self.web = Some(web);
        self
    }
    /// `web_search`/`web_fetch` for a D64 fork chat; unavailable to reviewers.
    pub(crate) async fn web(
        &self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<String, EngineError> {
        let Some(web) = &self.web else {
            return Err(EngineError::bad(ALLOWED));
        };
        let mut op = PreparedOperation::metadata(call.clone(), self.resolver.cwd.clone(), None);
        op.agent_id = web.agent_id.clone();
        op.breadcrumb = web.breadcrumb.clone();
        let config = web.config.lock().unwrap().config.web.clone();
        crate::tools::web::execute(web.gate.as_ref(), op, &config, (web.blocked)(), cancel)
            .await
            .map(|output| crate::tools::cap(&output))
            .map_err(|e| EngineError::bad(format!("{e:#}")))
    }
    pub async fn call(
        &self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<String, EngineError> {
        let mut call = call.clone();
        call.name = self.aliases.canonical(&call.name).to_owned();
        if !self.available(&call.name) {
            return Err(EngineError::bad(format!(
                "tool {} is unavailable",
                self.aliases.configured(&call.name)
            )));
        }
        if call.name == "script" {
            return self.script(&call.arguments, cancel).await;
        }
        self.read(&call, cancel).await
    }
    async fn read(
        &self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<String, EngineError> {
        let mut call = call.clone();
        call.name = self.aliases.canonical(&call.name).to_owned();
        if !self.available(&call.name)
            || !call.complete
            || !matches!(call.name.as_str(), "read" | "grep" | "glob")
        {
            return Err(EngineError::bad(ALLOWED));
        }
        let path = if call.name == "read" {
            call.arguments["path"]
                .as_str()
                .and_then(|s| self.resolver.resolve(s).ok())
                .filter(|path| self.roots.iter().any(|root| path.starts_with(root)))
        } else {
            None
        };
        let before = path.as_ref().and_then(|path| digest(path));
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let result = self
            .tools
            .execute(
                call.clone(),
                cancel.clone(),
                Arc::new(CommitBarrier::default()),
                &tx,
            )
            .await;
        if result.is_error {
            return Err(EngineError::bad(result.output));
        }
        if let (Some(path), Some(hash)) = (path, before)
            && digest(&path).as_deref() == Some(&hash)
        {
            let mut receipts = self.receipts.lock().unwrap();
            if !receipts
                .iter()
                .any(|r| r.path == path && r.hash == hash && r.args == call.arguments)
            {
                receipts.push(Receipt {
                    path,
                    hash,
                    args: call.arguments.clone(),
                    output: result.output.clone(),
                });
            }
        }
        Ok(result.output)
    }
    async fn script(
        &self,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<String, EngineError> {
        self.fork_script(args, 0, &[], cancel)
            .await
            .map(|(output, _)| output)
    }
    pub(crate) async fn fork_script(
        &self,
        args: &Value,
        index: usize,
        functions: &[crate::hidden_fork::ForkFunction],
        cancel: &CancellationToken,
    ) -> Result<(String, bool), EngineError> {
        if args["background"] == true {
            return Err(EngineError::bad(ALLOWED));
        }
        let code = args["code"]
            .as_str()
            .ok_or_else(|| EngineError::bad("script requires code"))?
            .to_owned();
        if code.len() > 1024 * 1024 {
            return Err(EngineError::bad("script is too large"));
        }
        let mut runtime = MontyRuntime::new(cancel.clone());
        let mut hosts = ["read", "grep", "glob"]
            .into_iter()
            .map(|name| HostFunction {
                name: name.into(),
                description: ALLOWED.into(),
            })
            .collect::<Vec<_>>();
        if self.checks.is_some() {
            hosts.push(HostFunction{name:"goal_check".into(),description:"Run an existing contract check by criterion ID through the session permission gate. goal_check(id=...).".into()});
        }
        if self.web.is_some() {
            hosts.extend(["web_search", "web_fetch"].map(|name| HostFunction {
                name: name.into(),
                description: "Web search / fetch (permission-gated)".into(),
            }));
        }
        hosts.retain(|h| {
            self.available(self.aliases.canonical(&h.name))
                || !["read", "grep", "glob", "web_search", "web_fetch"]
                    .contains(&self.aliases.canonical(&h.name))
        });
        let configured = hosts
            .iter()
            .filter_map(|h| {
                let name = self.aliases.configured(&h.name);
                (name != h.name).then(|| HostFunction {
                    name: name.into(),
                    description: h.description.clone(),
                })
            })
            .collect::<Vec<_>>();
        hosts.extend(configured);
        hosts.extend(functions.iter().map(|f| HostFunction {
            name: f.name.clone(),
            description: f.description.clone(),
        }));
        runtime
            .load_host_functions(hosts)
            .map_err(|e| EngineError::bad(e.to_string()))?;
        let mut output = Capture(Vec::new());
        let (mut runtime, mut output, mut progress) = tokio::task::spawn_blocking(move || {
            let result = runtime.run(
                &code,
                ScriptLimits {
                    cpu_secs: 1,
                    max_calls: 32,
                },
                &mut output,
            );
            (runtime, output, result)
        })
        .await
        .map_err(|e| EngineError::bad(e.to_string()))?;
        let mut called = false;
        for _ in 0..32 {
            if cancel.is_cancelled() {
                return Err(EngineError::cancelled());
            }
            match progress {
                Ok(ScriptProgress::Complete(value)) => {
                    return Ok((
                        json!({"stdout":String::from_utf8_lossy(&output.0),"value":value})
                            .to_string(),
                        called,
                    ));
                }
                Ok(ScriptProgress::Call { tool, args }) => {
                    let call = ToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: self.aliases.canonical(&tool).into(),
                        arguments: args,
                        complete: true,
                    };
                    let result = if let Some(function) =
                        functions.iter().find(|f| f.name == call.name)
                    {
                        let result = (function.call)(index, call.arguments);
                        called |= result.is_ok();
                        result
                    } else if call.name == "goal_check" {
                        if let Some(checks) = &self.checks {
                            checks
                                .run(&call.arguments, cancel)
                                .await
                                .map(|text| {
                                    serde_json::from_str(&text).unwrap_or(Value::String(text))
                                })
                                .map_err(|e| e.message)
                        } else {
                            Err(ALLOWED.into())
                        }
                    } else if crate::tools::web::handles(&call.name) {
                        self.web(&call, cancel)
                            .await
                            .map(Value::String)
                            .map_err(|e| e.message)
                    } else {
                        self.read(&call, cancel)
                            .await
                            .map(|text| serde_json::from_str(&text).unwrap_or(Value::String(text)))
                            .map_err(|e| e.message)
                    };
                    (runtime, output, progress) = tokio::task::spawn_blocking(move || {
                        let result = runtime.resume(result, &mut output);
                        (runtime, output, result)
                    })
                    .await
                    .map_err(|e| EngineError::bad(e.to_string()))?;
                }
                Err(error) => return Err(EngineError::bad(format!("{error}. {ALLOWED}"))),
            }
        }
        Err(EngineError::bad(format!(
            "script call limit reached. {ALLOWED}"
        )))
    }
}
struct Capture(Vec<u8>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > 8192 {
            return Err(std::io::Error::other("script output limit reached"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct CheckAccess {
    pub checks: Vec<(String, crate::goal::CriterionCheck)>,
    pub gate: Arc<dyn PermissionGate>,
    pub tools: ToolBox,
    pub cwd: PathBuf,
    pub control: Arc<crate::Control>,
    pub agent_id: String,
    pub barrier: Arc<CommitBarrier>,
}
impl CheckAccess {
    async fn run(&self, args: &Value, cancel: &CancellationToken) -> Result<String, EngineError> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| EngineError::bad("goal_check requires criterion id"))?;
        let check = &self
            .checks
            .iter()
            .find(|(key, _)| key == id)
            .ok_or_else(|| EngineError::bad("goal_check may run only an existing contract check"))?
            .1;
        let cwd = crate::paths::Resolver::new(self.cwd.clone())
            .resolve(check.cwd.as_ref().and_then(|p| p.to_str()).unwrap_or("."))
            .map_err(|e| EngineError::bad(e.to_string()))?;
        crate::paths::Resolver::new(self.cwd.clone())
            .validate(&cwd)
            .map_err(|e| EngineError::bad(e.to_string()))?;
        if !cwd.starts_with(&self.cwd) {
            let call = ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "read".into(),
                arguments: json!({"path":cwd}),
                complete: true,
            };
            let mut op = PreparedOperation::metadata(call, self.cwd.clone(), Some(cwd.clone()));
            op.agent_id = self.agent_id.clone();
            op.breadcrumb = self.agent_id.clone();
            self.gate
                .authorize(&op, cancel)
                .await
                .map_err(|e| EngineError::bad(e.to_string()))?;
        }
        let tools = self
            .tools
            .fork_for_agent(self.agent_id.clone(), self.agent_id.clone(), cwd);
        let call = ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: "bash".into(),
            arguments: json!({"command":check.command,"timeout_secs":check.timeout_secs.unwrap_or(120)}),
            complete: true,
        };
        self.control.commit(
            &self.agent_id,
            crate::schema::HistoryEvent::ToolCall { call: call.clone() },
        )?;
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let result = tools
            .execute(call, cancel.clone(), self.barrier.clone(), &tx)
            .await;
        self.control.commit(
            &self.agent_id,
            crate::schema::HistoryEvent::ToolResult {
                details: Default::default(),
                id: result.canonical_call.id,
                name: "bash".into(),
                output: result.output.clone(),
                is_error: result.is_error,
            },
        )?;
        Ok(json!({"passed":!result.is_error,"outputTail":result.output.chars().rev().take(4096).collect::<String>().chars().rev().collect::<String>()}).to_string())
    }
}
