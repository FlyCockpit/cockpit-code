//! Backend-neutral suspension boundary and the Monty adapter, lifted from Cockpit's bridge.
use crate::{
    State,
    model::EventSender,
    schema::*,
    tools::{self, CommitBarrier, ToolDefinition},
};
use anyhow::{Result, bail};
use monty::{FunctionCall, MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MontyException, MontyObject, MontyUuid,
    NameLookupResult, ObjectRef, PrintWriter, PrintWriterCallback, ResourceLimits, ResourceTracker,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{borrow::Cow, collections::BTreeMap, io::Write, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostFunction {
    pub name: String,
    pub description: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ScriptLimits {
    pub cpu_secs: u64,
    pub max_calls: usize,
}
impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            cpu_secs: 10,
            max_calls: 1000,
        }
    }
}
#[derive(Debug)]
pub enum ScriptProgress {
    Call { tool: String, args: Value },
    Complete(Value),
}
/// A backend owns the suspended VM. All host calls are serviced outside this trait.
/// No backend may invoke tools, filesystem or network directly.
pub trait ScriptRuntime: Send {
    fn load_host_functions(&mut self, functions: Vec<HostFunction>) -> Result<()>;
    fn run(
        &mut self,
        code: &str,
        limits: ScriptLimits,
        output: &mut dyn Write,
    ) -> Result<ScriptProgress>;
    fn resume(
        &mut self,
        result: Result<Value, String>,
        output: &mut dyn Write,
    ) -> Result<ScriptProgress>;
}
pub struct MontyRuntime {
    functions: Vec<HostFunction>,
    pending: Option<FunctionCall>,
    cancel: CancellationToken,
    callable: BTreeMap<MontyUuid, String>,
    remaining_suspensions: usize,
}
impl MontyRuntime {
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            functions: vec![],
            pending: None,
            cancel,
            callable: BTreeMap::new(),
            remaining_suspensions: 1000,
        }
    }
}
struct Printer<'a> {
    output: &'a mut dyn Write,
    cancel: &'a CancellationToken,
}
impl PrintWriterCallback for Printer<'_> {
    fn stdout_write(&mut self, value: Cow<'_, str>) -> Result<(), MontyException> {
        self.poll_flush()?;
        self.output
            .write_all(value.as_bytes())
            .map_err(|e| MontyException::new(ExcType::RuntimeError, Some(e.to_string())))
    }
    fn stdout_push(&mut self, value: char) -> Result<(), MontyException> {
        self.stdout_write(Cow::Owned(value.to_string()))
    }
    fn poll_flush(&mut self) -> Result<(), MontyException> {
        if self.cancel.is_cancelled() {
            return Err(MontyException::new(
                ExcType::RuntimeError,
                Some("script cancelled".into()),
            ));
        }
        Ok(())
    }
}
fn json_value(value: ObjectRef<'_>, depth: usize) -> Result<Value> {
    if depth > 64 {
        bail!("script value is cyclic or exceeds 64 levels");
    }
    if value.type_name() == "NoneType" {
        return Ok(Value::Null);
    }
    if let Some(v) = value.as_bool() {
        return Ok(json!(v));
    }
    if let Some(v) = value.as_int() {
        return Ok(json!(v));
    }
    if let Some(v) = value.as_float() {
        if !v.is_finite() {
            bail!("script values must be finite JSON numbers");
        }
        return Ok(json!(v));
    }
    if let Some(v) = value.as_str() {
        return Ok(json!(v));
    }
    if let Some(pairs) = value.pairs() {
        let mut out = serde_json::Map::new();
        for (k, v) in pairs {
            out.insert(
                k.as_str()
                    .ok_or_else(|| anyhow::anyhow!("tool arguments require string dict keys"))?
                    .into(),
                json_value(v, depth + 1)?,
            );
        }
        return Ok(Value::Object(out));
    }
    if let Some(items) = value.items() {
        return Ok(Value::Array(
            items
                .into_iter()
                .map(|v| json_value(v, depth + 1))
                .collect::<Result<_>>()?,
        ));
    }
    bail!("script value {} is not JSON-compatible", value.type_name())
}
fn monty_value(value: Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::none(),
        Value::Bool(v) => MontyObject::bool(v),
        Value::Number(v) => {
            if let Some(n) = v.as_i64() {
                MontyObject::int(n)
            } else {
                MontyObject::float(v.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(v) => MontyObject::string(v),
        Value::Array(v) => MontyObject::list(v.into_iter().map(monty_value)),
        Value::Object(v) => MontyObject::dict(
            v.into_iter()
                .map(|(k, v)| (MontyObject::string(k), monty_value(v))),
        ),
    }
}
#[derive(Default)]
struct Namespace {
    function: Option<HostFunction>,
    children: BTreeMap<String, Namespace>,
}
impl Namespace {
    fn object(
        self,
        name: &str,
        path: &str,
        callable: &mut BTreeMap<MontyUuid, String>,
    ) -> MontyObject {
        if self.children.is_empty()
            && let Some(f) = self.function
        {
            return MontyObject::function(f.name, Some(f.description));
        }
        let mut attrs = self
            .children
            .into_iter()
            .map(|(n, child)| {
                let object = child.object(&n, &format!("{path}.{n}"), callable);
                (MontyObject::string(n), object)
            })
            .collect::<Vec<_>>();
        let id = MontyUuid::from_bytes(*uuid::Uuid::new_v4().as_bytes());
        if let Some(f) = self.function {
            callable.insert(id, f.name.clone());
            attrs.push((
                MontyObject::string("__call__"),
                MontyObject::function(f.name, Some(f.description)),
            ));
        }
        let class = MontyObject::class_type(
            name,
            MontyUuid::from_bytes(*uuid::Uuid::new_v4().as_bytes()),
            true,
            false,
            [],
        );
        MontyObject::class_instance(class, id, attrs)
    }
}
impl MontyRuntime {
    fn progress(
        &mut self,
        mut progress: RunProgress,
        output: &mut dyn Write,
    ) -> Result<ScriptProgress> {
        loop {
            if !matches!(progress, RunProgress::Complete(_)) {
                if self.remaining_suspensions == 0 {
                    bail!("script suspension limit exceeded");
                }
                self.remaining_suspensions -= 1;
            }
            match progress {
                RunProgress::Complete(value) => {
                    return Ok(ScriptProgress::Complete(
                        json_value(value.as_ref(), 0)
                            .unwrap_or_else(|_| json!(value.as_ref().py_repr())),
                    ));
                }
                RunProgress::FunctionCall(call) => {
                    let mut tool = call.function_name.clone();
                    if tool == "__call__"
                        && let Some(name) = call.object_id.and_then(|id| self.callable.get(&id))
                    {
                        tool = name.clone();
                    }
                    let mut args = serde_json::Map::new();
                    for (k, v) in call.args.kwargs() {
                        args.insert(
                            k.as_str()
                                .ok_or_else(|| anyhow::anyhow!("keyword name must be a string"))?
                                .into(),
                            json_value(v, 0)?,
                        );
                    }
                    let positional = call
                        .args
                        .args()
                        .map(|v| json_value(v, 0))
                        .collect::<Result<Vec<_>>>()?;
                    if !positional.is_empty() {
                        let keys: &[&str] = match tool.as_str() {
                            "tools.search" | "mcp.search" => &["query"],
                            "tools.describe" | "mcp.describe" => &["name"],
                            "answer_question" => &["questionId", "answers", "summary"],
                            "btw_search" => &["chatId", "query"],
                            "store" => &["key", "value"],
                            "load" => &["key"],
                            _ => bail!("{tool} requires keyword arguments"),
                        };
                        if positional.len() > keys.len() {
                            bail!("{tool} takes at most {} positional arguments", keys.len());
                        }
                        for (key, value) in keys.iter().zip(positional) {
                            if args.contains_key(*key) {
                                bail!("{tool} received {key} twice");
                            }
                            args.insert((*key).into(), value);
                        }
                    }
                    self.pending = Some(call);
                    return Ok(ScriptProgress::Call {
                        tool,
                        args: Value::Object(args),
                    });
                }
                RunProgress::NameLookup(lookup) => {
                    let mut printer = Printer {
                        output,
                        cancel: &self.cancel,
                    };
                    progress = lookup.resume(
                        NameLookupResult::Undefined,
                        PrintWriter::Callback(&mut printer),
                    )?;
                }
                RunProgress::OsCall(call) => {
                    let mut printer = Printer {
                        output,
                        cancel: &self.cancel,
                    };
                    progress=call.resume(ExtFunctionResult::Error(MontyException::new(ExcType::RuntimeError,Some("script OS access requires builtin tools such as read(path=...) or bash(command=...)".into()))),PrintWriter::Callback(&mut printer))?;
                }
                RunProgress::ResolveFutures(_) => {
                    bail!("pending futures unsupported; use sequential host calls")
                }
            }
        }
    }
}
impl ScriptRuntime for MontyRuntime {
    fn load_host_functions(&mut self, functions: Vec<HostFunction>) -> Result<()> {
        self.functions = functions;
        Ok(())
    }
    fn run(
        &mut self,
        code: &str,
        limits: ScriptLimits,
        output: &mut dyn Write,
    ) -> Result<ScriptProgress> {
        self.remaining_suspensions = limits.max_calls;
        let mut root = Namespace::default();
        for f in &self.functions {
            let mut node = &mut root;
            for part in f.name.split('.') {
                node = node.children.entry(part.into()).or_default();
            }
            node.function = Some(f.clone());
        }
        let mut names = vec![];
        let mut values = vec![];
        for (name, node) in root.children {
            values.push(node.object(&name, &name, &mut self.callable));
            names.push(name);
        }
        let runner = MontyRun::new(code.into(), "script.py", names, CompileOptions::default())?;
        let limits = ResourceLimits::default()
            .max_feed_duration(Duration::from_secs(limits.cpu_secs.max(1)))
            .max_turn_duration(Duration::from_secs(limits.cpu_secs.max(1)));
        let mut printer = Printer {
            output,
            cancel: &self.cancel,
        };
        let progress = runner.start(
            values,
            ResourceTracker::new(limits),
            PrintWriter::Callback(&mut printer),
        )?;
        self.progress(progress, output)
    }
    fn resume(
        &mut self,
        result: Result<Value, String>,
        output: &mut dyn Write,
    ) -> Result<ScriptProgress> {
        let call = self
            .pending
            .take()
            .ok_or_else(|| anyhow::anyhow!("script is not suspended"))?;
        let result = match result {
            Ok(v) => ExtFunctionResult::Return(monty_value(v)),
            Err(e) => ExtFunctionResult::Error(MontyException::new(ExcType::RuntimeError, Some(e))),
        };
        let mut printer = Printer {
            output,
            cancel: &self.cancel,
        };
        let progress = call.resume(result, PrintWriter::Callback(&mut printer))?;
        self.progress(progress, output)
    }
}

pub(crate) fn definition() -> ToolDefinition {
    ToolDefinition { name:"script".into(),description:"Batch/filter harness calls in bounded Monty Python; each VM starts fresh; store(key,value)/load(key) retain agent-local JSON between calls (1 MiB, 1024 keys; cleared at agent end/restart). Use bash for OS commands. No host imports/packages or top-level return. Functions use keyword arguments; action tools have methods. Known tool: call directly or inspect tools.describe(name=...) for its schema. tools.search(query=...) returns a list of dicts; tools.describe returns a dict with schema. grep(pattern=...) searches files. mcp.search/describe are discovery aliases. Extensions: graph for relationship shortlists, context_pack for scoped maps, change_impact for explicit diff impact, available lsp for semantic identity, web_search/web_fetch for external pages, models for unfamiliar child models, decide/decisions for lasting choices. Native code finds definitions/outlines; history recovers exchanges. Inspect a schema with tools.describe(name='graph'), then call graph(kind='deps',path='src/main.ts'). Batch/filter before printing bounded answers; skills.search/read finds/reads skills. MCP: mcp.SERVER.TOOL(...). Inner calls retain permissions. background returns jobId for jobs output/wait/kill; CPU/call budgets exclude approval waits. Canonical names remain aliases.".into(),schema:json!({"type":"object","properties":{"code":{"type":"string"},"background":{"type":"boolean"},"cpuSecs":{"type":"integer","minimum":1},"maxCalls":{"type":"integer","minimum":1}},"required":["code"],"additionalProperties":false}) }
}
fn definitions(state: &State) -> Vec<ToolDefinition> {
    state.available_definitions()
}
fn skill_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "skills.search".into(),
            description: "Search the frozen skill catalog by name and description.".into(),
            schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: "skills.read".into(),
            description: "Read the current SKILL.md contents for a catalog skill name.".into(),
            schema: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}),
        },
    ]
}
fn functions(defs: &[ToolDefinition], names: &crate::tool_names::Names) -> Vec<HostFunction> {
    let mut functions = defs
        .iter()
        .map(|d| HostFunction {
            name: d.name.clone(),
            description: d.description.clone(),
        })
        .collect::<Vec<_>>();
    for d in defs {
        if matches!(
            names.canonical(&d.name),
            "criteria" | "history" | "jobs" | "models" | "scope" | "agents" | "decisions"
        ) && let Some(actions) = d.schema["properties"]["action"]["enum"].as_array()
        {
            for action in actions.iter().filter_map(Value::as_str) {
                functions.push(HostFunction {
                    name: format!("{}.{action}", d.name),
                    description: d.description.clone(),
                });
            }
        }
    }
    functions.push(HostFunction {
        name: "tools.search".into(),
        description: "Discover allowed tools only: tools.search(query='semantic references', limit=10) returns a list of name/description dicts. Does not search files; use grep(pattern=...) for repository text.".into(),
    });
    functions.push(HostFunction {
        name: "tools.describe".into(),
        description: "Inspect one allowed schema with tools.describe(name='...'), then call that function with keyword arguments.".into(),
    });
    // Unambiguous metadata aliases, never direct MCP tool execution.
    functions.push(HostFunction {
        name: "mcp.search".into(),
        description:
            "Alias of tools.search(query=...); discovers tools, never searches repository files."
                .into(),
    });
    functions.push(HostFunction {
        name: "mcp.describe".into(),
        description: "Alias of tools.describe(name=...); inspect an allowed schema.".into(),
    });
    functions.extend(skill_definitions().into_iter().map(|d| HostFunction {
        name: d.name,
        description: d.description,
    }));
    if !functions.iter().any(|function| function.name == "store") {
        functions.push(HostFunction { name: "store".into(), description: "store(key, value): keep a JSON value for this agent across script calls. 1 MiB serialized map, 1024 keys, keys 1–256 bytes; cleared at agent end or process restart.".into() });
    }
    if !functions.iter().any(|function| function.name == "load") {
        functions.push(HostFunction { name: "load".into(), description: "load(key): copy this agent's stored JSON value; None if missing. Python locals do not persist.".into() });
    }
    functions
}
impl State {
    pub(crate) fn for_script(&self, job_id: Option<String>) -> Self {
        Self {
            store: self.store.clone(),
            tools: self.tools.for_job(job_id.clone()),
            gate: self.gate.clone(),
            config: self.config.clone(),
            control: self.control.clone(),
            cwd: self.cwd.clone(),
            anchor: self.anchor,
            agent_id: self.agent_id.clone(),
            remaining_depth: self.remaining_depth,
            model_override: self.model_override.clone(),
            effort_override: self.effort_override.clone(),
            compact_request: self.compact_request.clone(),
            job_id,
            script_depth: self.script_depth + 1,
            parent_call_id: self.parent_call_id.clone(),
            warned_context: self.warned_context,
            compacted_estimate: self.compacted_estimate,
            child_profile: self.child_profile.clone(),
            continuing: self.continuing,
            guidance_turn: self.guidance_turn.clone(),
            guidance_notices: Arc::default(),
            context_state: self.context_state.clone(),
            active_model: self.active_model.clone(),
        }
    }
}
struct Output {
    text: String,
    foreground: Option<Arc<crate::foreground::Output>>,
    job: Option<Arc<crate::jobs::JobHandle>>,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.text.len() + bytes.len() > 10 * 1024 * 1024 {
            return Err(std::io::Error::other("script print output exceeds 10 MiB"));
        }
        if let Some(job) = &self.job {
            job.append(bytes)?;
        }
        if let Some(foreground) = &self.foreground {
            foreground.append(bytes)?;
        }
        self.text.push_str(&String::from_utf8_lossy(bytes));
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) async fn execute(
    state: &State,
    call: &ToolCall,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
    tx: &EventSender,
) -> Result<String, EngineError> {
    let args = &call.arguments;
    if cancel.is_cancelled() {
        return Err(EngineError::cancelled());
    }
    if state.script_depth >= 4 {
        return Err(EngineError::bad("script nesting exceeds 4"));
    }
    let code = args["code"]
        .as_str()
        .ok_or_else(|| EngineError::bad("script code required"))?
        .to_owned();
    if code.len() > 1024 * 1024 {
        return Err(EngineError::bad("script source exceeds 1 MiB"));
    }
    let config = state.config.lock().unwrap().config.script.clone();
    let limits = ScriptLimits {
        cpu_secs: args["cpuSecs"].as_u64().unwrap_or(config.cpu_secs),
        max_calls: args["maxCalls"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(config.max_calls),
    };
    if limits.cpu_secs == 0 || limits.max_calls == 0 {
        return Err(EngineError::bad("script limits must be positive"));
    }
    if args["background"] == true {
        let token = CancellationToken::new();
        let barrier = Arc::new(CommitBarrier::default());
        let job = state.control.start_job(
            &state.agent_id,
            "script",
            code.clone(),
            None,
            token.clone(),
            barrier.clone(),
        )?;
        let mut nested = state.for_script(Some(job.id.clone()));
        nested.parent_call_id = Some(call.id.clone());
        nested.tools.parent_call_id = nested.parent_call_id.clone();
        let control = state.control.clone();
        let worker = job.clone();
        // A session-owned job must not retain the launching turn's relay sender.
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let relay_control = control.clone();
        let owner = state.agent_id.clone();
        let relay = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                relay_control.emit_for(&owner, event);
            }
        });
        let task = tokio::spawn(async move {
            let result = run(
                &nested,
                code,
                limits,
                &token,
                barrier,
                &tx,
                Some(worker.clone()),
            )
            .await;
            drop(tx);
            let _ = relay.await;
            let status = if token.is_cancelled() {
                "killed"
            } else if result.is_ok() {
                "done"
            } else {
                "failed"
            };
            let report = result.unwrap_or_else(|e| format!("Error: {e}"));
            let _ = control.finish_job(&worker.id, status, report, None);
        });
        *job.task.lock().await = Some(task);
        Ok(json!({"jobId":job.id,"status":"running"}).to_string())
    } else {
        let mut nested = state.for_script(state.job_id.clone());
        nested.parent_call_id = Some(call.id.clone());
        nested.tools.parent_call_id = nested.parent_call_id.clone();
        run(&nested, code, limits, cancel, barrier, tx, None).await
    }
}
async fn run(
    state: &State,
    code: String,
    limits: ScriptLimits,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
    tx: &EventSender,
    job: Option<Arc<crate::jobs::JobHandle>>,
) -> Result<String, EngineError> {
    let defs = definitions(state);
    let names = state.tool_names();
    let mut hosts = functions(&defs, &names);
    let aliases = defs
        .iter()
        .filter(|d| names.canonical(&d.name) != d.name)
        .cloned()
        .map(|mut d| {
            d.name = names.canonical(&d.name).into();
            d
        })
        .collect::<Vec<_>>();
    let alias_hosts = functions(&aliases, &names);
    for host in alias_hosts {
        if !hosts.iter().any(|h| h.name == host.name) {
            hosts.push(host);
        }
    }
    // Catalog errors stay local to that server so builtins remain available.
    let mut catalog = vec![];
    for server in state
        .control
        .mcp
        .statuses()
        .into_iter()
        .filter(|s| s.enabled)
    {
        match state.control.mcp.catalog(&server.name, cancel).await {
            Ok(tools) => {
                for tool in tools {
                    let name = format!("mcp.{}.{}", server.name, tool.name);
                    hosts.push(HostFunction {
                        name: name.clone(),
                        description: tool.description.clone(),
                    });
                    catalog.push(ToolDefinition {
                        name,
                        description: tool.description,
                        schema: tool.input_schema,
                    });
                }
            }
            Err(e) => {
                catalog.push(ToolDefinition {
                    name: format!("mcp.{}", server.name),
                    description: e.to_string(),
                    schema: Value::Null,
                });
            }
        }
    }
    let mut discovery = defs.clone();
    discovery.extend(catalog);
    discovery.extend(skill_definitions());
    let mut runtime: Box<dyn ScriptRuntime> = Box::new(MontyRuntime::new(cancel.clone()));
    runtime
        .load_host_functions(hosts)
        .map_err(|e| EngineError::bad(e.to_string()))?;
    let mut output = Output {
        foreground: state.tools.foreground.clone(),
        text: String::new(),
        job,
    };
    let max_calls = limits.max_calls;
    let (r, o, p) = tokio::task::spawn_blocking(move || {
        let p = runtime.run(&code, limits, &mut output);
        (runtime, output, p)
    })
    .await
    .map_err(|e| EngineError::bad(e.to_string()))?;
    runtime = r;
    output = o;
    let mut progress = p;
    let mut count = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(EngineError::cancelled());
        }
        match progress {
            Err(e) => {
                return Err(EngineError::bad(format!(
                    "{e}\n{}",
                    tools::cap(&output.text)
                )));
            }
            Ok(ScriptProgress::Complete(value)) => {
                if let Some(job) = &output.job {
                    let _ = job.append(format!("\nResult: {value}\n").as_bytes());
                }
                let notices = std::mem::take(&mut *state.guidance_notices.lock().unwrap());
                let mut result = json!({"stdout":output.text,"value":value});
                if notices.is_empty() {
                    return Ok(result.to_string());
                }
                // Preserve guidance even when a script did not print the write's return value.
                let ordinary = result.to_string();
                result =
                    serde_json::from_str(&ordinary).unwrap_or_else(|_| json!({"output":ordinary}));
                result["guidance"] = json!(notices);
                return Ok(result.to_string());
            }
            Ok(ScriptProgress::Call { tool, args }) => {
                count += 1;
                if count > max_calls {
                    return Err(EngineError::bad("script host-call limit exceeded"));
                }
                let summary = tools::cap(&args.to_string())
                    .chars()
                    .take(256)
                    .collect::<String>();
                let id = uuid::Uuid::new_v4().to_string();
                let (base, method) = tool
                    .split_once('.')
                    .map_or((tool.as_str(), None), |(b, m)| (b, Some(m)));
                let call = ToolCall {
                    id: id.clone(),
                    name: format!(
                        "{}{}",
                        names.configured(base),
                        method.map(|m| format!(".{m}")).unwrap_or_default()
                    ),
                    arguments: args.clone(),
                    complete: true,
                };
                let emit = |result: Option<ToolResultData>| -> Result<(), EngineError> {
                    state.control.commit(
                        &state.agent_id,
                        HistoryEvent::ScriptCall {
                            parent_call_id: state.parent_call_id.clone(),
                            call: call.clone(),
                            result: result.clone(),
                        },
                    )?;
                    state.control.emit(EngineEvent::ScriptCall {
                        call: call.clone(),
                        parent_call_id: state.parent_call_id.clone(),
                        job_id: state.job_id.clone(),
                        agent_id: state.agent_id.clone(),
                        tool: call.name.clone(),
                        args_summary: summary.clone(),
                        status: result
                            .as_ref()
                            .map_or(
                                "started",
                                |r| if r.is_error { "error" } else { "completed" },
                            )
                            .into(),
                        result,
                    });
                    Ok(())
                };
                emit(None)?;
                let started = std::time::Instant::now();
                let mut details = ToolResultDetails::default();
                let result = dispatch(
                    state,
                    &discovery,
                    &tool,
                    &id,
                    args,
                    cancel,
                    barrier.clone(),
                    tx,
                    &mut details,
                )
                .await;
                details.duration_ms =
                    Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
                emit(Some(ToolResultData {
                    output: match &result {
                        Ok(Value::String(text)) => text.clone(),
                        Ok(v) => v.to_string(),
                        Err(e) => e.clone(),
                    },
                    is_error: result.is_err(),
                    details,
                }))?;
                let (r, o, p) = tokio::task::spawn_blocking(move || {
                    let p = runtime.resume(result, &mut output);
                    (runtime, output, p)
                })
                .await
                .map_err(|e| EngineError::bad(e.to_string()))?;
                runtime = r;
                output = o;
                progress = p;
            }
        }
    }
}
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    state: &State,
    defs: &[ToolDefinition],
    name: &str,
    call_id: &str,
    mut args: Value,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
    tx: &EventSender,
    details: &mut ToolResultDetails,
) -> Result<Value, String> {
    let names = state.tool_names();
    let (base, method) = name
        .split_once('.')
        .map_or((name, None), |(b, m)| (b, Some(m)));
    let canonical_name = format!(
        "{}{}",
        names.canonical(base),
        method.map(|m| format!(".{m}")).unwrap_or_default()
    );
    let name = match canonical_name.as_str() {
        "mcp.search" => "tools.search",
        "mcp.describe" => "tools.describe",
        other => other,
    };
    let result: Result<Value> = async {
        state.control.check_owner()?;
        if cancel.is_cancelled() {
            bail!("script cancelled");
        }
        if name == "store" || name == "load" {
            let key = args["key"].as_str().ok_or_else(|| anyhow::anyhow!("key must be a string"))?;
            if key.is_empty() || key.len() > 256 { bail!("store keys must be 1–256 bytes"); }
            let mut agents = state.control.script_values.lock().unwrap();
            if name == "load" {
                return Ok(agents.get(&state.agent_id).and_then(|values| values.get(key)).cloned().unwrap_or(Value::Null));
            }
            if args.as_object().is_none_or(|args| !args.contains_key("value")) { bail!("value required"); }
            let mut values = agents.get(&state.agent_id).cloned().unwrap_or_default();
            values.insert(key.into(), args["value"].clone());
            if values.len() > 1024 || serde_json::to_vec(&values)?.len() > 1024 * 1024 {
                bail!("agent script store exceeds 1 MiB or 1024 keys; previous values retained");
            }
            agents.insert(state.agent_id.clone(), values);
            return Ok(Value::Null);
        }
        if state.agent_id=="root" && state.control.goal_state().is_some_and(|g|g.phase=="drafting") {
            let base=name.split('.').next().unwrap_or(name);
            if !matches!(base,"read"|"read_image"|"read_video"|"grep"|"glob"|"history"|"models"|"agents"|"decisions"|"criteria"|"script"|"skills"|"tools") {
                bail!("Goal drafting is read-only; MCP, bash, write/edit and task calls are unavailable. Use read, grep, glob, history, models or criteria draft actions.");
            }
        }
        if name == "skills.search" || name == "skills.read" {
            let context = state
                .context_state
                .frozen
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("skill catalog is not initialized"))?;
            if name == "skills.search" {
                let query = args["query"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("query required"))?;
                let call = ToolCall {
                    id: call_id.into(),
                    name: name.into(),
                    arguments: args.clone(),
                    complete: true,
                };
                state
                    .gate
                    .authorize(&state.metadata_operation(call, None), cancel)
                    .await?;
                return Ok(json!(context.search(query)));
            }
            let skill_name = args["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("name required"))?;
            let skill = context
                .skills
                .iter()
                .find(|s| s.name == skill_name)
                .ok_or_else(|| anyhow::anyhow!("skill not found: {skill_name}"))?;
            crate::skill_commands::model_allowed(skill).map_err(|e|anyhow::anyhow!(e.message))?;
            if skill.disable_model_invocation { bail!("Skill {skill_name} is user-only (disable-model-invocation); ask the user to invoke it."); }
            // Use the ordinary read boundary, including outside-cwd approval and
            // no-follow path validation. No direct skill tool exists today.
            let call = ToolCall {
                id: call_id.into(),
                name: "read".into(),
                arguments: json!({"path":skill.path,"limit":10000}),
                complete: true,
            };
            let result = crate::execute(state, call, cancel.clone(), barrier, tx).await;
            *details = result.details;
        if result.is_error {
                bail!("{}", result.output);
            }
            return Ok(json!({"name":skill.name,"path":skill.path,"content":result.output}));
        }
        if name == "tools.search" || name == "tools.describe" {
            let call = ToolCall {
                id: call_id.into(),
                name: name.into(),
                arguments: args.clone(),
                complete: true,
            };
            state
                .gate
                .authorize(&state.metadata_operation(call, None), cancel)
                .await?;
            let text_route = defs.iter().find(|d| names.canonical(&d.name) == "grep")
                .map(|d| format!("For repository text call {}(pattern='...', path='...', glob='...').", d.name))
                .unwrap_or_else(|| "No repository text tool is available to this agent.".into());
            if name == "tools.search" && args.get("pattern").is_some() {
                bail!("tools.search is capability discovery, not file execution: tools.search(query='semantic references'). {text_route} Native code finds syntax definitions/outlines. Inspect tools.describe(name='...') before calling a needed function.");
            }
            let key = if name == "tools.search" { "query" } else { "name" };
            let allowed: &[&str] = if name == "tools.search" { &["query","limit"] } else { &["name"] };
            if args.as_object().is_some_and(|o|o.keys().any(|k|!allowed.contains(&k.as_str()))) {
                bail!("Use tools.search(query='...', limit=10) to discover capabilities or tools.describe(name='...') for a schema. {text_route}");
            }
            let query = args[key].as_str().ok_or_else(||anyhow::anyhow!("Use tools.search(query='...') for capability discovery or tools.describe(name='...') for a schema. {text_route}"))?.to_lowercase();
            if name == "tools.describe" {
                return defs.iter().find(|d| d.name.to_lowercase() == query || names.canonical(&d.name).to_lowercase() == query)
                    .map(|d|json!(d)).ok_or_else(|| {
                        if query == "search" { anyhow::anyhow!("Repository search is retired in new sessions. {text_route} Use tools.search(query='...') only to discover allowed tools.") }
                        else { anyhow::anyhow!("Tool unavailable: {query}. Discover allowed names with tools.search(query='...'); inspect with tools.describe(name='...').") }
                    });
            }
            let cap = args.get("limit").map(|v|v.as_u64().filter(|n|(1..=100).contains(n)).ok_or_else(||anyhow::anyhow!("tools.search limit must be 1..100"))).transpose()?.unwrap_or(10) as usize;
            let terms = query.split(|c:char| !c.is_alphanumeric() && c != '_').filter(|t|!t.is_empty()).collect::<Vec<_>>();
            let mut ranked = defs.iter().filter_map(|d| {
                let canonical = names.canonical(&d.name).to_lowercase();
                let name = d.name.to_lowercase();
                let text = format!("{canonical} {name} {}", d.description).to_lowercase();
                let score = if query.is_empty() { 1 }
                    else if name == query || canonical == query { 1000 }
                    else { terms.iter().map(|t| if name.contains(t) || canonical.contains(t) { 3 } else if text.contains(t) { 1 } else { 0 }).sum() };
                (score>0).then_some((score,d))
            }).collect::<Vec<_>>();
            ranked.sort_by(|(a,da),(b,db)| b.cmp(a).then_with(||da.name.cmp(&db.name)));
            return Ok(Value::Array(ranked.into_iter().take(cap).map(|(_,d)|json!({"name":d.name,"description":d.description})).collect()));
        }
        if let Some(rest) = name.strip_prefix("mcp.") {
            // Resolve longest configured server prefix, preserving dots in wire tool names.
            let server = state
                .control
                .mcp
                .statuses()
                .into_iter()
                .filter(|s| rest.starts_with(&format!("{}.", s.name)))
                .max_by_key(|s| s.name.len())
                .ok_or_else(|| anyhow::anyhow!("MCP server not found"))?;
            let tool = &rest[server.name.len() + 1..];
            let call = ToolCall {
                id: call_id.into(),
                name: "mcp".into(),
                arguments: json!({"server":server.name,"tool":tool,"arguments":args}),
                complete: true,
            };
            let mut op = state.metadata_operation(call, None);
            let mut note = None;
            if let Some((revised, reason)) = state.gate.revise(&op, true, cancel).await? {
                op = state.metadata_operation(revised, None);
                note = Some(reason);
            }
            state.gate.authorize(&op, cancel).await?;
            let value = state
                .control
                .mcp
                .call(
                    &server.name,
                    tool,
                    op.call.arguments["arguments"].clone(),
                    cancel,
                )
                .await?;
            if value["isError"] == true {
                bail!("MCP tool error: {value}");
            }
            state.control.goal_progress.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
            let result = value.get("structuredContent").cloned().unwrap_or_else(|| {
                if let Some(content) = value["content"].as_array() {
                    if content.iter().any(|block| block["type"] != "text") {
                        return value;
                    }
                    Value::String(
                        content
                            .iter()
                            .filter_map(|block| block["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                } else {
                    value
                }
            });
            return Ok(if let Some(note) = note {
                json!({"result":result,"variationNote":note})
            } else {
                result
            });
        }
        let (base, action) = name
            .split_once('.')
            .map_or((name, None), |(b, a)| (b, Some(a)));
        if !defs.iter().any(|d| names.canonical(&d.name) == base) {
            if base == "search" && let Some(text) = defs.iter().find(|d|names.canonical(&d.name)=="grep") {
                bail!("Repository search is retired in new sessions. Use {}(pattern='...', path='...', glob='...') for text; tools.search(query='...') discovers capabilities.",text.name);
            }
            bail!("tool {name} is unavailable to this agent; use tools.search(query='...') for allowed capabilities, then tools.describe(name='...')");
        }
        if let Some(action) = action {
            if !matches!(base, "jobs" | "models" | "criteria" | "history" | "scope" | "agents" | "decisions") {
                bail!("unknown builtin method");
            }
            args["action"] = json!(action);
        }
        let call = ToolCall {
            id: call_id.into(),
            name: base.into(),
            arguments: args,
            complete: true,
        };
        if !matches!(
            base,
            "read"
                | "write"
                | "edit"
                | "grep"
                | "glob"
                | "bash"
                | "todo"
                | "script"
                | "lsp"
                | "search"
                | "code"
                | "graph"
                | "context_pack"
                | "change_impact"
                | "web_search"
                | "web_fetch"
                | "generate"
                | "agents"
                | "decide"
                | "decisions"
        ) {
            state
                .gate
                .authorize(&state.metadata_operation(call.clone(), None), cancel)
                .await?;
        }
        let result = crate::execute(state, call, cancel.clone(), barrier, tx).await;
        *details = result.details;
        if result.is_error {
            bail!("{}", result.output);
        }
        Ok(serde_json::from_str(&result.output).unwrap_or(Value::String(result.output)))
    }
    .await;
    result.map_err(|e| e.to_string())
}
