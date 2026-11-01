//! Builtin tools. Every dispatch passes through PermissionGate; policy is external to tools.
mod bash_hints;
mod data_syntax;
mod edit_match;
pub(crate) mod generate;
pub(crate) mod lsp;
pub(crate) mod media;
mod paging;
pub(crate) mod shell_compress;
pub(crate) mod web;
use crate::{
    model::EventSender,
    schema::{EditHunk, EngineEvent, FileEdit, HistoryEvent, ToolCall, ToolResultDetails},
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, sinks};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub schema: Value,
}
/// Authorization sees exactly the target, canonical arguments and diff execution will use.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedOperation {
    pub requested: ToolCall,
    pub call: ToolCall,
    pub cwd: PathBuf,
    /// Shell execution directory; cwd remains the agent's permission boundary.
    pub execution_cwd: Option<PathBuf>,
    pub target: Option<PathBuf>,
    pub diff: Option<String>,
    pub command: Option<String>,
    pub spans: Vec<Value>,
    pub agent_id: String,
    pub breadcrumb: String,
    pub job_id: Option<String>,
    pub parent_call_id: Option<String>,
    pub edits: Vec<FileEdit>,
    #[serde(skip)]
    expected: Option<Vec<u8>>,
    #[serde(skip)]
    bytes: Option<Vec<u8>>,
    #[serde(skip)]
    note: String,
}
impl PreparedOperation {
    pub(crate) fn expected_digest(&self) -> Option<String> {
        self.expected
            .as_ref()
            .map(|hash| hash.iter().map(|b| format!("{b:02x}")).collect())
    }
    pub fn metadata(call: ToolCall, cwd: PathBuf, target: Option<PathBuf>) -> Self {
        let mut canonical = call.clone();
        if let Some(path) = &target
            && matches!(
                call.name.as_str(),
                "read" | "write" | "edit" | "grep" | "glob"
            )
        {
            canonical.arguments["path"] = json!(path);
        }
        if let Some(path) = &target {
            if call.name == "bash" {
                canonical.arguments["cwd"] = json!(path);
            } else if call.name == "lsp" {
                canonical.arguments["file"] = json!(path);
            } else if crate::intel::NAMES.contains(&call.name.as_str()) {
                if call.name != "context_pack" {
                    canonical.arguments["path"] = json!(path);
                } else if call.arguments["kind"] == "path"
                    || (call.arguments["kind"]
                        .as_str()
                        .is_none_or(|kind| kind == "auto")
                        && call.arguments["target"].as_str().is_some_and(|raw| {
                            crate::paths::Resolver::new(cwd.clone())
                                .resolve(raw)
                                .is_ok_and(|p| p == *path && p.exists())
                        }))
                {
                    canonical.arguments["target"] = json!(path);
                }
            }
        }
        Self {
            execution_cwd: (call.name == "bash").then(|| target.clone()).flatten(),
            command: call.arguments["command"].as_str().map(str::to_owned),
            requested: call,
            call: canonical,
            cwd,
            target,
            diff: None,
            spans: vec![],
            agent_id: "root".into(),
            breadcrumb: "root".into(),
            job_id: None,
            parent_call_id: None,
            edits: vec![],
            expected: None,
            bytes: None,
            note: String::new(),
        }
    }
    pub fn outside(&self) -> bool {
        self.target
            .as_ref()
            .is_some_and(|p| !p.starts_with(&self.cwd))
    }
    pub fn effective_cwd(&self) -> &Path {
        self.execution_cwd.as_deref().unwrap_or(&self.cwd)
    }
}
#[async_trait]
pub trait PermissionGate: Send + Sync {
    /// Candidate selection precedes authorization; the dispatcher re-prepares every revision.
    async fn revise(
        &self,
        _operation: &PreparedOperation,
        _improve: bool,
        _cancel: &CancellationToken,
    ) -> Result<Option<(ToolCall, String)>> {
        Ok(None)
    }
    async fn authorize(
        &self,
        operation: &PreparedOperation,
        cancel: &CancellationToken,
    ) -> Result<()>;
    /// Holds any session ownership guard across the final filesystem replacement.
    fn commit(&self, action: Box<dyn FnOnce() -> Result<()> + Send + '_>) -> Result<()> {
        action()
    }
}
pub struct YoloGate;
#[async_trait]
impl PermissionGate for YoloGate {
    async fn authorize(
        &self,
        _operation: &PreparedOperation,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        Ok(())
    }
}
/// Cancellation and the last check+rename take this same short barrier.
#[derive(Default)]
pub struct CommitBarrier {
    revoked: Mutex<bool>,
    parent: Mutex<Option<Arc<CommitBarrier>>>,
}
impl CommitBarrier {
    pub(crate) fn linked(parent: Arc<Self>) -> Self {
        Self {
            revoked: Mutex::new(false),
            parent: Mutex::new(Some(parent)),
        }
    }
    pub(crate) fn detach(&self) -> Result<()> {
        let revoked = self.revoked.lock().unwrap();
        if *revoked {
            bail!("cancelled before background adoption");
        }
        let mut parent = self.parent.lock().unwrap();
        if let Some(parent) = &*parent {
            parent.commit(|| Ok(()))?;
        }
        parent.take();
        Ok(())
    }
    pub fn revoke(&self) {
        *self.revoked.lock().unwrap() = true;
    }
    pub(crate) fn commit<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let revoked = self.revoked.lock().unwrap();
        if *revoked {
            bail!("cancelled before commit");
        }
        let parent = self.parent.lock().unwrap();
        if let Some(parent) = &*parent {
            parent.commit(action)
        } else {
            action()
        }
    }
}
fn path_barrier(path: &Path) -> Arc<Mutex<()>> {
    static PATHS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut paths = PATHS.get_or_init(Mutex::default).lock().unwrap();
    paths.retain(|_, weak| weak.strong_count() > 0);
    if let Some(lock) = paths.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    paths.insert(path.to_owned(), Arc::downgrade(&lock));
    lock
}
type RecentWrites = HashMap<PathBuf, (String, Vec<u8>)>;
#[derive(Clone)]
pub struct ToolBox {
    pub(crate) control: Weak<crate::Control>,
    scope_permit: Option<Arc<crate::scopes::ScopePermit>>,
    pub(crate) cwd: PathBuf,
    pub(crate) resolver: crate::paths::Resolver,
    agent_id: String,
    breadcrumb: String,
    job_id: Option<String>,
    pub(crate) parent_call_id: Option<String>,
    structured: bool,
    pub(crate) foreground: Option<Arc<crate::foreground::Output>>,
    gate: Arc<dyn PermissionGate>,
    reads: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
    read_contents: Arc<Mutex<crate::file_notices::ReadMemory>>,
    own_writes: Arc<Mutex<RecentWrites>>,
    todos: Arc<Mutex<Value>>,
    pub(crate) lsp: Arc<lsp::Manager>,
    pub(crate) intel: Arc<crate::intel::Manager>,
    bash_history: Arc<Mutex<HashMap<String, bash_hints::History>>>,
    pub(crate) names: Arc<Mutex<crate::tool_names::Names>>,
}
pub struct ToolOutput {
    pub details: ToolResultDetails,
    pub output: String,
    pub is_error: bool,
    pub canonical_call: ToolCall,
}
impl ToolBox {
    pub fn new(cwd: PathBuf, gate: Arc<dyn PermissionGate>) -> Self {
        Self {
            control: Weak::new(),
            scope_permit: None,
            resolver: crate::paths::Resolver::new(cwd.clone()),
            agent_id: "root".into(),
            breadcrumb: "root".into(),
            job_id: None,
            parent_call_id: None,
            structured: false,
            foreground: None,
            cwd,
            gate,
            reads: Arc::default(),
            read_contents: Arc::default(),
            own_writes: Arc::default(),
            todos: Arc::new(Mutex::new(json!([]))),
            lsp: Arc::default(),
            intel: Arc::default(),
            bash_history: Arc::default(),
            names: Arc::default(),
        }
    }
    /// Children inherit a snapshot of read revisions; later sibling reads cannot authorize stale writes.
    pub fn fork_for_agent(&self, agent_id: String, breadcrumb: String, cwd: PathBuf) -> Self {
        Self {
            control: self.control.clone(),
            scope_permit: None,
            resolver: crate::paths::Resolver::new(cwd.clone()),
            cwd,
            agent_id,
            breadcrumb,
            job_id: None,
            parent_call_id: None,
            structured: false,
            foreground: None,
            gate: self.gate.clone(),
            reads: Arc::new(Mutex::new(self.reads.lock().unwrap().clone())),
            read_contents: Arc::new(Mutex::new(self.read_contents.lock().unwrap().clone())),
            own_writes: self.own_writes.clone(),
            todos: Arc::new(Mutex::new(json!([]))),
            lsp: self.lsp.clone(),
            intel: self.intel.clone(),
            bash_history: self.bash_history.clone(),
            names: Arc::new(Mutex::new(self.names.lock().unwrap().clone())),
        }
    }
    pub(crate) fn for_job(&self, job_id: Option<String>) -> Self {
        let mut tools = self.clone();
        tools.job_id = job_id;
        tools.structured = true;
        tools
    }
    /// Observe revisions without authorizing a stale write or advancing read state.
    pub(crate) fn context_observations(&self) -> (Value, Vec<(PathBuf, String, String)>) {
        let reads = self.reads.lock().unwrap().clone();
        let mut changed = reads
            .into_iter()
            .filter_map(|(path, expected)| match read_bytes(&path) {
                Ok(bytes) if hash(&bytes) == expected => None,
                Ok(bytes) => Some((
                    path.clone(),
                    hash(&bytes).iter().map(|b| format!("{b:02x}")).collect(),
                    self.read_contents.lock().unwrap().notice(&path),
                )),
                Err(_) => Some((
                    path.clone(),
                    "unreadable or removed".into(),
                    format!("{} removed or unreadable; re-read it.", path.display()),
                )),
            })
            .collect::<Vec<_>>();
        changed.sort_by(|a, b| a.0.cmp(&b.0));
        (self.todos.lock().unwrap().clone(), changed)
    }
    pub(crate) fn watch_source(&self, path: &Path, bytes: &[u8]) {
        self.read_contents.lock().unwrap().remember(path, bytes);
    }
    fn mutation_snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut paths = self
            .reads
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        paths.extend(self.read_contents.lock().unwrap().paths());
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .filter_map(|p| read_bytes(&p).ok().map(|b| (p, hash(&b))))
            .collect()
    }
    fn remember_mutations(&self, before: Vec<(PathBuf, Vec<u8>)>, command: &str) {
        for (path, old) in before {
            // Attribute explicit shell redirections only. Formatters and git remain
            // disk changes, including when launched from a shell tool.
            let named = [
                path.to_string_lossy().to_string(),
                path.strip_prefix(&self.cwd)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ];
            if !named.iter().filter(|p| !p.is_empty()).any(|p| {
                [
                    format!(">{p}"),
                    format!("> {p}"),
                    format!(">'{p}'"),
                    format!("> '{p}'"),
                    format!(">\"{p}\""),
                    format!("> \"{p}\""),
                ]
                .iter()
                .any(|p| command.contains(p))
            }) {
                continue;
            }
            if let Ok(bytes) = read_bytes(&path)
                && hash(&bytes) != old
            {
                self.reads
                    .lock()
                    .unwrap()
                    .insert(path.clone(), hash(&bytes));
                self.remember_write(&path, &bytes);
            }
        }
    }
    pub(crate) fn read_notice(&self, path: &Path) -> Option<String> {
        let memory = self.read_contents.lock().unwrap();
        memory
            .paths()
            .iter()
            .any(|p| p == path)
            .then(|| memory.notice(path))
    }
    pub(crate) fn own_revision(&self, path: &Path) -> bool {
        self.own_writes
            .lock()
            .unwrap()
            .get(path)
            .is_some_and(|(id, revision)| {
                id == &self.agent_id && read_bytes(path).is_ok_and(|b| hash(&b) == *revision)
            })
    }
    fn remember_write(&self, path: &Path, bytes: &[u8]) {
        self.read_contents.lock().unwrap().remember(path, bytes);
        let mut writes = self.own_writes.lock().unwrap();
        if writes.len() >= 4096 {
            writes.clear();
        }
        writes.insert(path.into(), (self.agent_id.clone(), hash(bytes)));
    }
    pub fn definitions() -> Vec<ToolDefinition> {
        let string = json!({"type":"string"});
        let number = json!({"type":"integer","minimum":1});
        let path = json!({"type":"string","x-cockpit-kind":"path","x-cockpit-aliases":["file_path","filePath","filepath","file","absolute_path"],"description":"Absolute path or path relative to cwd"});
        let def = |name: &str, description: &str, props: Value, required: Value| ToolDefinition {
            name: name.into(),
            description: description.into(),
            schema: json!({"type":"object","properties":props,"required":required,"additionalProperties":false}),
        };
        let mut definitions = vec![
            def(
                "read",
                "Read UTF-8 text with line numbers; use read_image for pixels. Read existing files before write/edit. Default 2000 lines within 8 KiB; continue with returned offset/start_byte.",
                json!({"path":path,"offset":number,"limit":number,"start_byte":{"type":"integer","minimum":0,"description":"UTF-8 byte cursor within the selected line; use the returned cursor"}}),
                json!(["path"]),
            ),
            def(
                "write",
                "Create or replace a whole file atomically; use edit for a matching region. Read existing files first; re-read if changed.",
                json!({"path":path,"content":string,"wait":{"type":"boolean","description":"Wait for an out-of-scope request; default false"}}),
                json!(["path", "content"]),
            ),
            def(
                "edit",
                "Change a matching region in a previously read file; use write for whole-file replacement. Include unique old_string context; matching has a fuzzy fallback.",
                json!({"path":path,"old_string":{"type":"string","x-cockpit-aliases":["oldString"]},"new_string":{"type":"string","x-cockpit-aliases":["newString"]},"replace_all":{"type":"boolean","x-cockpit-aliases":["replaceAll"]},"wait":{"type":"boolean","description":"Wait for an out-of-scope request; default false"}}),
                json!(["path", "old_string", "new_string"]),
            ),
            def(
                "bash",
                "Run OS commands/processes; script orchestrates harness calls/discovery. Fresh sh -c; optional cwd resolves against your agent cwd. Scope and permission rules are unchanged; no shell state persists. Default timeout 120s, allowed 1–600s; output previews compressed. Large output is saved to a session file for read/grep. tty:true uses a PTY; otherwise pipes. yieldMs (0–30000; tty default 1000) returns output so far and jobId when still running. background:true yields immediately. These jobs have no timeout and end with you; jobs.input(jobId,chars) sends input, empty chars polls; jobs output/wait/kill collects them.",
                json!({"command":{"type":"string","x-cockpit-aliases":["cmd"]},"timeout_secs":{"type":"integer","minimum":1,"maximum":600,"x-cockpit-aliases":["timeoutSecs"]},"background":{"type":"boolean"},"tty":{"type":"boolean"},"yieldMs":{"type":"integer","minimum":0,"maximum":30000},"cwd":{"type":"string","description":"Working directory, absolute or relative to agent cwd; default agent cwd"}}),
                json!(["command"]),
            ),
            def(
                "grep",
                "Search repository text/regex lines in UTF-8 files or saved output; use glob for paths, code for definitions/outlines. Respects ignores. Returns path:line:text within 8 KiB; offset is the 1-based match index, start_byte pages long matches.",
                json!({"pattern":string,"path":path,"glob":string,"case_sensitive":{"type":"boolean"},"offset":number,"start_byte":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000}}),
                json!(["pattern"]),
            ),
            def(
                "glob",
                "Find file paths by glob, not file contents (use grep). Relative to path (default cwd); respects ignores. Page with returned 1-based offset/start_byte.",
                json!({"pattern":string,"path":path,"offset":number,"start_byte":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000}}),
                json!(["pattern"]),
            ),
            def(
                "todo",
                "Replace the session todo list with items having content and pending/in_progress/completed status.",
                json!({"items":{"type":"array","items":{"type":"object","properties":{"content":string,"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["content","status"],"additionalProperties":false}}}),
                json!(["items"]),
            ),
        ];
        definitions.extend(media::definitions());
        definitions.push(lsp::definition());
        definitions.extend(crate::intel::definitions());
        definitions.push(crate::user_terminal::definition());
        definitions
    }
    pub fn read_only(name: &str) -> bool {
        matches!(
            name,
            "read" | "grep" | "glob" | "history" | "lsp" | "skill" | "read_image" | "read_video"
        ) || crate::intel::NAMES.contains(&name)
    }
    /// Action-aware dispatch classification. Mutating actions stay as barriers.
    pub fn parallel(call: &ToolCall) -> bool {
        match call.name.as_str() {
            "web_fetch" | "web_search" => true,
            "models" => matches!(
                call.arguments["action"].as_str(),
                Some("list" | "rank" | "describe")
            ),
            "decisions" => matches!(call.arguments["action"].as_str(), Some("list" | "search")),
            "agents" => matches!(
                call.arguments["action"].as_str(),
                Some("list" | "show" | "ask")
            ),
            "criteria" => matches!(call.arguments["action"].as_str(), Some("show" | "query")),
            "jobs" => matches!(
                call.arguments["action"].as_str(),
                Some("list" | "status" | "output")
            ),
            "history" => matches!(
                call.arguments["action"].as_str(),
                Some("query" | "search" | "contexts" | "summaries" | "read" | "get")
            ),
            "lsp" => matches!(
                call.arguments["operation"].as_str(),
                Some(
                    "definition"
                        | "references"
                        | "hover"
                        | "document_symbols"
                        | "workspace_symbols"
                        | "diagnostics"
                        | "rename_preview"
                )
            ),
            name => Self::read_only(name),
        }
    }
    pub fn target(&self, call: &ToolCall) -> Result<Option<PathBuf>> {
        if call.name == "bash" {
            return call.arguments["cwd"]
                .as_str()
                .map(|raw| self.resolver.resolve(raw))
                .transpose();
        }
        if call.name == "lsp" {
            return Ok(Some(
                self.resolver
                    .resolve(call.arguments["file"].as_str().unwrap_or("."))?,
            ));
        }
        if crate::intel::NAMES.contains(&call.name.as_str()) {
            return Ok(Some(crate::intel::target(&self.resolver, call)?));
        }
        if matches!(
            call.name.as_str(),
            "read" | "write" | "edit" | "grep" | "glob"
        ) {
            let raw = call.arguments["path"]
                .as_str()
                .or_else(|| matches!(call.name.as_str(), "grep" | "glob").then_some("."))
                .context("path is required")?;
            return Ok(Some(self.resolver.resolve(raw)?));
        }
        Ok(None)
    }
    pub async fn execute(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        barrier: Arc<CommitBarrier>,
        tx: &EventSender,
    ) -> ToolOutput {
        let mut canonical = call.clone();
        let mut details = ToolResultDetails::default();
        let result: Result<String> = async {
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            let mut prepared =
                PreparedOperation::metadata(call, self.cwd.clone(), self.target(&canonical)?);
            prepared.agent_id = self.agent_id.clone();
            prepared.breadcrumb = self.breadcrumb.clone();
            prepared.job_id = self.job_id.clone();
            prepared.parent_call_id = self.parent_call_id.clone();
            if let Some(control) = self.control.upgrade() {
                let dir = control.outputs_dir();
                if prepared
                    .target
                    .as_ref()
                    .is_some_and(|p| p.starts_with(&dir))
                    && matches!(prepared.call.name.as_str(), "write" | "edit" | "generate")
                {
                    bail!("session output files are read-only; writes there are refused");
                }
            }
            // An outside-cwd edit needs a read decision before reading bytes to compute its diff.
            if prepared.outside()
                && matches!(prepared.call.name.as_str(), "write" | "edit")
                && prepared.target.as_ref().is_some_and(|p| p.exists())
            {
                let mut read = prepared.clone();
                read.call.name = "read".into();
                read.requested.name = "read".into();
                read.call.arguments = json!({"path": read.target});
                read.requested.arguments = read.call.arguments.clone();
                self.gate.authorize(&read, &cancel).await?;
            }
            if matches!(prepared.call.name.as_str(), "write" | "edit") {
                let this = self.clone();
                let token = cancel.clone();
                prepared =
                    tokio::task::spawn_blocking(move || this.prepare_write(prepared, &token))
                        .await??;
            }
            canonical = prepared.call.clone();
            if let Some(output) = self.check_write_scope(&prepared, &cancel).await? {
                return Ok(output);
            }
            let mut variation_notes = Vec::new();
            for improve in [false, true] {
                if let Some((call, note)) = self.gate.revise(&prepared, improve, &cancel).await? {
                    prepared = PreparedOperation::metadata(
                        call.clone(),
                        self.cwd.clone(),
                        self.target(&call)?,
                    );
                    prepared.agent_id = self.agent_id.clone();
                    prepared.breadcrumb = self.breadcrumb.clone();
                    prepared.job_id = self.job_id.clone();
                    prepared.parent_call_id = self.parent_call_id.clone();
                    if prepared.outside()
                        && matches!(prepared.call.name.as_str(), "write" | "edit")
                        && prepared.target.as_ref().is_some_and(|p| p.exists())
                    {
                        let mut read = prepared.clone();
                        read.call.name = "read".into();
                        read.requested.name = "read".into();
                        read.call.arguments = json!({"path":read.target});
                        self.gate.authorize(&read, &cancel).await?;
                    }
                    if matches!(prepared.call.name.as_str(), "write" | "edit") {
                        let this = self.clone();
                        let token = cancel.clone();
                        prepared = tokio::task::spawn_blocking(move || {
                            this.prepare_write(prepared, &token)
                        })
                        .await??;
                    }
                    canonical = prepared.call.clone();
                    variation_notes.push(note);
                }
            }
            if let Some(output) = self.check_write_scope(&prepared, &cancel).await? {
                return Ok(output);
            }
            self.gate.authorize(&prepared, &cancel).await?;
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            if let Some(path) = &prepared.target {
                self.resolver.validate(path)?;
            }
            if let Some(foreground) = &self.foreground
                && foreground
                    .canonical
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|call| call.id == prepared.call.id)
            {
                *foreground.canonical.lock().unwrap() = Some(prepared.call.clone());
            }
            let output = match prepared.call.name.as_str() {
                "lsp" => self.lsp(&prepared.call.arguments, &cancel).await,
                name if crate::intel::NAMES.contains(&name) => {
                    self.intelligence(&prepared.call, &cancel).await
                }
                "bash" => {
                    if prepared.call.arguments["background"] == true
                        || prepared.call.arguments["tty"] == true
                        || prepared.call.arguments.get("yieldMs").is_some()
                    {
                        self.interactive_bash(&prepared.call, &cancel).await
                    } else {
                        let before = self.mutation_snapshot();
                        let result = self
                            .run_bash(&prepared.call.id, &prepared.call.arguments, &cancel, None)
                            .await;
                        self.remember_mutations(
                            before,
                            prepared.call.arguments["command"].as_str().unwrap_or(""),
                        );
                        result
                    }
                }
                "todo" => {
                    let items = prepared.call.arguments["items"].clone();
                    *self.todos.lock().unwrap() = items.clone();
                    let _ = tx.send(EngineEvent::Todo {
                        items: items.clone(),
                    });
                    Ok(items.to_string())
                }
                "write" | "edit" => {
                    let path = prepared.target.clone().context("missing write path")?;
                    let text =
                        String::from_utf8(prepared.bytes.clone().context("missing write bytes")?)?;
                    let before = self
                        .lsp
                        .before_write(&self.cwd, &path, &self.agent_id, &cancel)
                        .await;
                    let edits = prepared.edits.clone();
                    let this = self.clone();
                    let output =
                        tokio::task::spawn_blocking(move || this.commit_write(prepared, &barrier))
                            .await??;
                    details.edits = edits;
                    let note = match self.lsp.after_write(before, &path, &text, &cancel).await {
                        Some(note) => note,
                        None => {
                            tokio::task::spawn_blocking(move || data_syntax::check(&path, &text))
                                .await
                                .unwrap_or_default()
                        }
                    };
                    Ok(format!("{output}{note}"))
                }
                _ => {
                    let this = self.clone();
                    let token = cancel.clone();
                    tokio::task::spawn_blocking(move || this.file_tool(&mut prepared.call, &token))
                        .await?
                }
            }?;
            Ok(if variation_notes.is_empty() {
                output
            } else {
                format!("{}\n{}", variation_notes.join("\n"), output)
            })
        }
        .await;
        match result {
            Ok(output) => ToolOutput {
                details,
                output: if self.structured && serde_json::from_str::<Value>(&output).is_ok() {
                    output
                } else {
                    cap(&output)
                },
                is_error: false,
                canonical_call: canonical,
            },
            Err(e) => ToolOutput {
                details: Default::default(),
                output: cap(&format!("Error: {e:#}")),
                is_error: true,
                canonical_call: canonical,
            },
        }
    }
    pub(crate) fn prepare_scope_apply(
        &mut self,
        request: &crate::scopes::ScopeRequest,
    ) -> Result<()> {
        self.agent_id = request.requester.clone();
        self.breadcrumb = self
            .control
            .upgrade()
            .map(|c| c.breadcrumb(&self.agent_id))
            .unwrap_or_default();
        self.job_id = request.job_id.clone();
        self.parent_call_id = request.parent_call_id.clone();
        if let Ok(bytes) = read_bytes(&request.path) {
            self.reads
                .lock()
                .unwrap()
                .insert(request.path.clone(), hash(&bytes));
        }
        self.scope_permit = Some(Arc::new(crate::scopes::ScopePermit {
            request_id: request.id.clone(),
            path: request.path.clone(),
        }));
        Ok(())
    }
    async fn check_write_scope(
        &self,
        op: &PreparedOperation,
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        let Some(control) = self.control.upgrade() else {
            return Ok(None);
        };
        if !matches!(op.call.name.as_str(), "write" | "edit") {
            return Ok(None);
        }
        if let Some(permit) = &self.scope_permit {
            if op.target.as_ref() != Some(&permit.path)
                || control
                    .journal
                    .lock()
                    .unwrap()
                    .scope_requests
                    .get(&permit.request_id)
                    .is_none_or(|r| {
                        r.status != "applying" || r.expected_hash != op.expected_digest()
                    })
            {
                bail!("scope request changed during approval; retry with a fresh request");
            }
            return Ok(None);
        }
        control
            .request_scope(op, cancel)
            .await
            .map_err(anyhow::Error::from)
    }
    fn path(&self, args: &Value, optional: bool) -> Result<PathBuf> {
        self.resolver.resolve(
            args["path"]
                .as_str()
                .or(optional.then_some("."))
                .context("path is required")?,
        )
    }
    /// Dry-run the ordinary edit cascade against the already authorized source.
    pub(crate) fn validate_combined_edit(
        op: &PreparedOperation,
        call: &mut ToolCall,
    ) -> Result<()> {
        if call.name != "edit" {
            return Ok(());
        }
        let resolver = crate::paths::Resolver::new(op.cwd.clone());
        let path = resolver.resolve(field(&call.arguments, "path")?)?;
        if op.target.as_ref() != Some(&path) {
            bail!("combined edits must retain the reviewed target");
        }
        resolver.validate(&path)?;
        let bytes = read_bytes(&path)?;
        let body = std::str::from_utf8(&bytes)?;
        let target = field(&call.arguments, "old_string")?;
        let matched = edit_match::find_match(
            body,
            target,
            call.arguments["replace_all"].as_bool().unwrap_or(false),
        )?
        .context("combined edit does not apply")?;
        let replacement = line_endings(field(&call.arguments, "new_string")?, &bytes);
        edit_match::replace_spans(body, &matched.spans, &replacement)?;
        call.arguments["old_string"] = json!(matched.canonical);
        call.arguments["new_string"] = json!(replacement);
        Ok(())
    }
    fn prepare_write(
        &self,
        mut prepared: PreparedOperation,
        cancel: &CancellationToken,
    ) -> Result<PreparedOperation> {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        let path = prepared
            .target
            .as_ref()
            .context("missing prepared target")?;
        self.resolver.validate(path)?;
        let old = if path.exists() {
            let bytes = read_bytes(path)?;
            self.check_stale(path, &bytes)?;
            Some(bytes)
        } else {
            None
        };
        let args = &prepared.call.arguments;
        let (body, note) = if prepared.call.name == "write" {
            (field(args, "content")?.to_owned(), "wrote file".to_owned())
        } else {
            let bytes = old.as_ref().context("file does not exist")?;
            let body = std::str::from_utf8(bytes).context("file is not UTF-8")?;
            let target = field(args, "old_string")?;
            let replacement = line_endings(field(args, "new_string")?, bytes);
            let matched = edit_match::find_match(
                body,
                target,
                args["replace_all"].as_bool().unwrap_or(false),
            )?
            .with_context(|| {
                format!(
                    "old_string not found; nearest region:\n{}",
                    cap(&edit_match::nearest_miss(body, target))
                )
            })?;
            let result = edit_match::replace_spans(body, &matched.spans, &replacement)?;
            prepared.spans = matched.spans.iter().map(|span| json!({"start":span.start,"end":span.end,"old":&body[span.clone()],"new":replacement})).collect();
            prepared.call.arguments["old_string"] = json!(matched.canonical);
            prepared.call.arguments["new_string"] = json!(replacement);
            (
                result,
                format!(
                    "edited {} occurrence(s), match stage {}",
                    matched.spans.len(),
                    matched.stage
                ),
            )
        };
        let body = old
            .as_ref()
            .map_or(body.clone(), |old| line_endings(&body, old));
        let before = old
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let diff = similar::TextDiff::from_lines(&before, &body)
            .unified_diff()
            .context_radius(3)
            .header(&path.to_string_lossy(), &path.to_string_lossy())
            .to_string();
        let changes = similar::TextDiff::from_lines(&before, &body);
        let old_lines = changes.old_slices();
        let new_lines = changes.new_slices();
        let hunks = changes
            .grouped_ops(3)
            .into_iter()
            .map(|ops| {
                let first = ops.first().unwrap();
                let last = ops.last().unwrap();
                let old = first.old_range().start..last.old_range().end;
                let new = first.new_range().start..last.new_range().end;
                EditHunk {
                    start_line: new.start + 1,
                    old_start_line: old.start + 1,
                    old_text: old_lines[old].concat(),
                    new_text: new_lines[new].concat(),
                }
            })
            .collect();
        prepared.edits = vec![FileEdit {
            path: path.clone(),
            hunks,
        }];
        prepared.diff = Some(diff);
        prepared.expected = old.as_ref().map(|b| hash(b));
        prepared.bytes = Some(body.into_bytes());
        prepared.note = note;
        Ok(prepared)
    }
    fn commit_write(&self, prepared: PreparedOperation, barrier: &CommitBarrier) -> Result<String> {
        let path = prepared.target.as_ref().context("missing target")?;
        let bytes = prepared.bytes.as_ref().context("missing prepared bytes")?;
        let parent = path.parent().context("path has no parent")?;
        // No closure is abandoned: even slow staging finishes before the turn acknowledges cancel.
        self.resolver.validate(path)?;
        fs::create_dir_all(parent)?;
        // New files get the ordinary 0666 & !umask mode; replacements keep theirs.
        let mut temp = new_file_temp(parent)?;
        if let Some(control) = self.control.upgrade() {
            control.subscription_stage(temp.path());
        }
        if prepared.expected.is_some() {
            temp.as_file()
                .set_permissions(fs::metadata(path)?.permissions())?;
        }
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        let path_lock = path_barrier(path);
        let _path_guard = path_lock.lock().unwrap();
        barrier.commit(|| {
            self.gate.commit(Box::new(|| {
                self.resolver.validate(path)?;
                match &prepared.expected {
                    Some(expected) if hash(&read_bytes(path)?) != *expected => {
                        bail!("file changed during approval/preparation; read it again")
                    }
                    None if path.exists() => {
                        bail!("file appeared during approval/preparation; read it first")
                    }
                    _ => {}
                }
                if prepared.expected.is_none() {
                    temp.persist_noclobber(path).map_err(|e| e.error)?;
                } else {
                    temp.persist(path).map_err(|e| e.error)?;
                }
                self.reads.lock().unwrap().insert(path.clone(), hash(bytes));
                self.remember_write(path, bytes);
                Ok(())
            }))
        })?;
        if let Some(control) = self.control.upgrade() {
            control.subscription_write_bytes(&self.agent_id, path, bytes)?;
            control.notice_parent_scope_write(&self.agent_id, path)?;
        }
        Ok(format!(
            "{}: {} ({} bytes)",
            prepared.note,
            path.display(),
            bytes.len()
        ))
    }
    fn file_tool(&self, call: &mut ToolCall, cancel: &CancellationToken) -> Result<String> {
        let args = &call.arguments;
        match call.name.as_str() {
            "read" => {
                let path = self.path(args, false)?;
                if path.is_dir() {
                    bail!(
                        "{} is a directory. Use glob with this path and pattern \"*\" to list files, or grep to search its contents.",
                        path.display()
                    );
                }
                let bytes = read_bytes(&path)?;
                if bytes.contains(&0) {
                    bail!("binary file; use bash for binary inspection")
                }
                let body = std::str::from_utf8(&bytes).context("file is not UTF-8")?;
                self.reads
                    .lock()
                    .unwrap()
                    .insert(path.clone(), hash(&bytes));
                self.read_contents.lock().unwrap().remember(&path, &bytes);
                let mut page = paging::Page::new(args, 2000);
                for (index, line) in body.lines().enumerate() {
                    if cancel.is_cancelled() {
                        bail!("cancelled");
                    }
                    if !page.push(&format!("{}: ", index + 1), line)? {
                        break;
                    }
                }
                let output = page.finish("[empty range]");
                Ok(output)
            }
            "grep" | "glob" => self.search(call, cancel),
            _ => bail!(
                "unknown tool {}; available: read, write, edit, bash, grep, glob, todo",
                call.name
            ),
        }
    }
    fn check_stale(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let seen = self.reads.lock().unwrap();
        let expected = seen
            .get(path)
            .context("read the file before overwriting it")?;
        if *expected != hash(bytes) {
            bail!("file changed since it was read; read it again")
        }
        Ok(())
    }
    fn search(&self, call: &ToolCall, cancel: &CancellationToken) -> Result<String> {
        let args = &call.arguments;
        let root = self.path(args, true)?;
        let mut page = paging::Page::new(args, 200);
        let mut more = true;
        let pattern = field(args, "pattern")?;
        let glob = if call.name == "glob" {
            Some(pattern)
        } else {
            args["glob"].as_str()
        };
        let filter = glob
            .map(globset::Glob::new)
            .transpose()?
            .map(|g| g.compile_matcher());
        let matcher = if call.name == "grep" {
            Some(
                RegexMatcherBuilder::new()
                    .case_insensitive(!args["case_sensitive"].as_bool().unwrap_or(true))
                    .build(pattern)?,
            )
        } else {
            None
        };
        let mut walk = WalkBuilder::new(&root);
        walk.hidden(true)
            .require_git(false)
            .sort_by_file_path(|a, b| a.cmp(b))
            .max_filesize(Some(32 * 1024 * 1024));
        let mut errors = Vec::new();
        for entry in walk.build() {
            if cancel.is_cancelled() {
                bail!("cancelled")
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    errors.push(e.to_string());
                    continue;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&root).unwrap_or(path);
            if filter.as_ref().is_some_and(|g| {
                !g.is_match(rel) && !g.is_match(path.file_name().unwrap_or_default())
            }) {
                continue;
            }
            if let Some(matcher) = &matcher {
                let mut searcher = SearcherBuilder::new()
                    .line_number(true)
                    .binary_detection(grep_searcher::BinaryDetection::quit(0))
                    .build();
                let bytes = match read_bytes(path) {
                    Ok(b) => b,
                    Err(e) => {
                        errors.push(format!("{}: {e}", path.display()));
                        continue;
                    }
                };
                if let Err(e) = searcher.search_slice(
                    matcher,
                    &bytes,
                    sinks::UTF8(|line, text| {
                        if cancel.is_cancelled() {
                            return Ok(false);
                        }
                        more = page
                            .push(&format!("{}:{line}:", path.display()), text.trim_end())
                            .map_err(std::io::Error::other)?;
                        Ok(more)
                    }),
                ) {
                    errors.push(format!("{}: {e}", path.display()));
                }
            } else {
                more = page.push("", &path.display().to_string())?;
            }
            if !more {
                break;
            }
        }
        let mut output = page.finish("[no matches in this range]");
        if !errors.is_empty() {
            output.push_str(&format!(
                "\n[{} unreadable entries; first: {}]",
                errors.len(),
                errors[0].chars().take(300).collect::<String>()
            ));
        }
        Ok(output)
    }
    pub(crate) async fn background_bash(
        &self,
        call_id: &str,
        args: &Value,
        cancel: &CancellationToken,
        job: Arc<crate::jobs::JobHandle>,
    ) -> Result<String> {
        let before = self.mutation_snapshot();
        let result = self.run_bash(call_id, args, cancel, Some(job)).await;
        self.remember_mutations(before, args["command"].as_str().unwrap_or(""));
        result
    }
    async fn interactive_bash(
        &self,
        call: &crate::schema::ToolCall,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let control = self
            .control
            .upgrade()
            .context("interactive commands require a session")?;
        let token = CancellationToken::new();
        let job = control.start_job(
            &self.agent_id,
            "bash",
            field(&call.arguments, "command")?.into(),
            None,
            token.clone(),
            Arc::new(CommitBarrier::default()),
        )?;
        control.guidance_job(&call.id, &job.id);
        let mut toolbox = self.clone();
        toolbox.foreground = None;
        let args = call.arguments.clone();
        let call_id = call.id.clone();
        let runner = job.clone();
        let owner = control.clone();
        *job.task.lock().await = Some(tokio::spawn(async move {
            let result = toolbox
                .background_bash(&call_id, &args, &token, runner.clone())
                .await;
            let status = if token.is_cancelled() {
                "killed"
            } else if result.is_ok() {
                "done"
            } else {
                "failed"
            };
            let report = result.unwrap_or_else(|e| format!("Error: {e:#}"));
            let _ = owner.finish_job(&runner.id, status, report, None);
        }));
        let wait = if call.arguments["background"] == true {
            0
        } else {
            call.arguments["yieldMs"]
                .as_u64()
                .unwrap_or(1000)
                .min(30000)
        };
        Ok(control.wait_job_yield(&job.id, wait, cancel).await?)
    }
    async fn run_bash(
        &self,
        call_id: &str,
        args: &Value,
        cancel: &CancellationToken,
        job: Option<Arc<crate::jobs::JobHandle>>,
    ) -> Result<String> {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let command = field(args, "command")?;
        let cwd = self.resolver.resolve(args["cwd"].as_str().unwrap_or("."))?;
        if !cwd.is_dir() {
            bail!("bash cwd must be an existing directory");
        }
        let resolver = crate::paths::Resolver::new(cwd.clone());
        let mut shell_writes = Vec::new();
        if let Some(control) = self.control.upgrade() {
            for raw in crate::subscriptions::shell_write_paths(command) {
                if let Ok(path) = resolver.resolve(&raw) {
                    shell_writes.push((path.clone(), crate::subscriptions::file_signature(&path)));
                }
            }
            let _ = control;
        }
        let secs = args["timeout_secs"].as_u64().unwrap_or(120).clamp(1, 600);
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .stdin(if job.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let pty = if args["tty"] == true {
            Some(crate::pty::Pty::open(&mut cmd)?)
        } else {
            cmd.as_std_mut().process_group(0);
            None
        };
        let mut child = cmd.spawn().context("start sh")?;
        let guard = ProcessGroup(child.id().context("missing child pid")? as i32);
        // Drain every raw byte to disk before bounded preview capture/compression.
        let spool = if job.is_none() {
            Some(Arc::new(Mutex::new(tempfile::tempfile()?)))
        } else {
            None
        };
        if let Some(job) = &job {
            if let Some(pty) = &pty {
                job.set_input(crate::jobs::Input::Pty(pty.clone())).await;
                if let Some(control) = self.control.upgrade() {
                    let terminal = crate::terminals::TerminalInfo {
                        id: job.id.clone(),
                        agent_id: self.agent_id.clone(),
                        job_id: Some(job.id.clone()),
                        cwd: cwd.clone(),
                        status: "running".into(),
                        kind: crate::terminals::TerminalKind::Job,
                        breadcrumb: self.breadcrumb.clone(),
                        prompt: None,
                        command: Some(command.into()),
                        share: crate::terminals::ShareMode::Output,
                        wait: None,
                        exit_status: None,
                        shared_selection_bytes: 0,
                    };
                    control
                        .terminals
                        .infos
                        .lock()
                        .unwrap()
                        .insert(job.id.clone(), terminal.clone());
                    *job.terminal_tx.lock().unwrap() = Some(control.terminals.bytes.clone());
                    control.emit(EngineEvent::TerminalChanged { terminal });
                }
            } else if let Some(stdin) = child.stdin.take() {
                job.set_input(crate::jobs::Input::Pipe(stdin)).await;
            }
        }
        let stdout: Box<dyn tokio::io::AsyncRead + Unpin + Send> = if let Some(pty) = &pty {
            Box::new(crate::pty::Reader(pty.clone()))
        } else {
            Box::new(child.stdout.take().unwrap())
        };
        let stderr: Box<dyn tokio::io::AsyncRead + Unpin + Send> = if pty.is_some() {
            Box::new(tokio::io::empty())
        } else {
            Box::new(child.stderr.take().unwrap())
        };
        let out_task = tokio::spawn(capture(
            stdout,
            job.clone(),
            self.foreground.clone(),
            spool.clone(),
        ));
        let err_task = tokio::spawn(capture(
            stderr,
            job.clone(),
            self.foreground.clone(),
            spool.clone(),
        ));
        let (status, reason) = tokio::select! {
            status = child.wait() => (status?,None),
            _ = cancel.cancelled() => { guard.signal(libc::SIGINT); let status = match tokio::time::timeout(Duration::from_secs(5),child.wait()).await { Ok(s) => s?, Err(_) => {guard.signal(libc::SIGKILL);child.wait().await?} }; (status,Some("cancelled")) },
            _ = async { if job.is_none() { if let Some(foreground) = &self.foreground {
                    tokio::select! { _ = foreground.backgrounded() => std::future::pending::<()>().await, _ = tokio::time::sleep(Duration::from_secs(secs)) => {} }
                } else { tokio::time::sleep(Duration::from_secs(secs)).await; } } else { std::future::pending::<()>().await; } } => { guard.signal(libc::SIGINT); let status = match tokio::time::timeout(Duration::from_secs(5),child.wait()).await { Ok(s) => s?, Err(_) => {guard.signal(libc::SIGKILL);child.wait().await?} }; (status,Some("timed out")) },
        };
        if let Some(job) = &job {
            job.input.lock().await.take();
            if let Some(control) = self.control.upgrade()
                && let Some(info) = control.terminals.infos.lock().unwrap().get_mut(&job.id)
            {
                info.exit_status = status.code();
            }
        }
        // Foreground shell ownership ends here, including descendants holding output pipes open.
        drop(guard);
        if let Some(control) = self.control.upgrade() {
            for (path, before) in shell_writes {
                if before != crate::subscriptions::file_signature(&path) {
                    let _ = control.subscription_write(&self.agent_id, &path);
                    let _ = control.notice_parent_scope_write(&self.agent_id, &path);
                }
            }
        }
        let stdout = out_task.await??;
        let stderr = err_task.await??;
        let mut output = format!(
            "exit: {}{}\nstdout:\n{}\nstderr:\n{}",
            status
                .code()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "signal".into()),
            reason.map(|s| format!(" ({s})")).unwrap_or_default(),
            shell_compress::compress_stream(command, &stdout),
            shell_compress::compress_stream(command, &stderr)
        );
        let hint = {
            let control = self.control.upgrade();
            let journal = control.as_ref().map(|c| c.journal.lock().unwrap());
            let context = journal
                .as_ref()
                .and_then(|j| j.contexts.get(&self.agent_id))
                .copied()
                .unwrap_or(0);
            let mut histories = self.bash_history.lock().unwrap();
            let history = histories.entry(self.agent_id.clone()).or_default();
            if history.context != context {
                let names = self.names.lock().unwrap();
                *history = bash_hints::History::restore(context, |tip| {
                    let tip = names.rewrite(&format!("[tip: {tip}]"));
                    journal
                        .as_ref()
                        .and_then(|j| j.histories.get(&self.agent_id))
                        .is_some_and(|events| {
                            events.iter().any(|event| match event {
                                HistoryEvent::ToolResult { output, .. } => output.contains(&tip),
                                HistoryEvent::ScriptCall {
                                    result: Some(result),
                                    ..
                                } => result.output.contains(&tip),
                                _ => false,
                            })
                        })
                });
            }
            drop(journal);
            bash_hints::append(
                history,
                command,
                status.success() && reason.is_none(),
                &stdout,
                &stderr,
            )
        };
        output.push_str(&self.names.lock().unwrap().rewrite(&hint));
        let raw_bytes = if let Some(spool) = &spool {
            spool.lock().unwrap().metadata()?.len()
        } else if let Some(job) = &job {
            self.control
                .upgrade()
                .and_then(|c| c.job_record(&job.id).ok())
                .map_or(0, |j| j.output_bytes)
        } else {
            0
        };
        if (raw_bytes > 8192 || output.len() > 8192)
            && let Some(control) = self.control.upgrade()
        {
            let path = if let Some(job) = &job {
                let record = control.job_record(&job.id)?;
                let source = record.output_path.clone().unwrap_or_else(|| {
                    control
                        .journal
                        .lock()
                        .unwrap()
                        .dir
                        .join("jobs")
                        .join(format!("{}.log", job.id))
                });
                let path = control.save_output(call_id, &mut std::fs::File::open(source)?)?;
                let mut record = record;
                record.output_path = Some(path.clone());
                control.commit(&self.agent_id, HistoryEvent::JobUpdate { job: record })?;
                path
            } else {
                let id = control.store_bash_output(
                    &self.agent_id,
                    command,
                    call_id,
                    &mut spool.as_ref().unwrap().lock().unwrap(),
                    status.success() && reason.is_none(),
                )?;
                control.job_record(&id)?.output_path.unwrap()
            };
            let status_line = output.lines().next().unwrap_or("").to_owned();
            output = format!("{status_line}\n{}", crate::outputs::preview(&path)?);
        }
        if status.success() && reason.is_none() {
            Ok(output)
        } else {
            bail!("{output}")
        }
    }
}
fn field<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .with_context(|| format!("{key} must be a string"))
}
fn hash(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}
pub(crate) fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    const MAX: u64 = 32 * 1024 * 1024;
    if !fs::metadata(path)?.is_file() {
        bail!("only regular files can be read");
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("read {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("only regular files can be read");
    }
    let mut bytes = Vec::new();
    file.take(MAX + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX {
        bail!("file exceeds 32 MiB; read a range with bash");
    }
    Ok(bytes)
}
fn line_endings(body: &str, old: &[u8]) -> String {
    if old.windows(2).any(|w| w == b"\r\n") {
        body.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        body.to_owned()
    }
}
/// A staging file created with mode 0666 filtered by the process umask, so a
/// file persisted from it looks like any newly created user file (tempfile's
/// default is 0600).
pub(crate) fn new_file_temp(dir: &Path) -> Result<tempfile::NamedTempFile> {
    use std::os::unix::fs::PermissionsExt;
    Ok(tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o666))
        .tempfile_in(dir)?)
}
pub fn cap(body: &str) -> String {
    cap_at(body, 8192)
}
fn cap_at(body: &str, limit: usize) -> String {
    if body.len() <= limit {
        return body.into();
    }
    let marker = format!("\n… [output truncated; {} bytes total] …\n", body.len());
    let budget = limit - marker.len();
    let mut head = budget * 3 / 5;
    let mut tail = body.len() - (budget - head);
    while !body.is_char_boundary(head) {
        head -= 1;
    }
    while !body.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{marker}{}", &body[..head], &body[tail..])
}
struct ProcessGroup(i32);
impl ProcessGroup {
    fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(-self.0, signal);
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}
async fn capture(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    job: Option<Arc<crate::jobs::JobHandle>>,
    foreground: Option<Arc<crate::foreground::Output>>,
    spool: Option<Arc<Mutex<std::fs::File>>>,
) -> std::io::Result<String> {
    // Bounded memory while continuing to drain a noisy child: 1 MiB head + 1 MiB tail.
    const HALF: usize = 1024 * 1024;
    let mut head = Vec::new();
    let mut tail = std::collections::VecDeque::new();
    let mut total = 0usize;
    let mut buf = [0; 8192];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if let Some(spool) = &spool {
            std::io::Write::write_all(&mut *spool.lock().unwrap(), &buf[..n])?;
        }
        if let Some(job) = &job {
            job.append(&buf[..n])?;
        }
        if let Some(foreground) = &foreground {
            foreground.append(&buf[..n])?;
        }
        total += n;
        for b in &buf[..n] {
            if head.len() < HALF {
                head.push(*b);
            } else {
                if tail.len() == HALF {
                    tail.pop_front();
                }
                tail.push_back(*b);
            }
        }
    }
    let mut result = String::from_utf8_lossy(&head).into_owned();
    if total > HALF * 2 {
        result.push_str(&format!(
            "\n… {} captured bytes elided …\n",
            total - HALF * 2
        ));
    }
    result.push_str(&String::from_utf8_lossy(
        &tail.into_iter().collect::<Vec<_>>(),
    ));
    Ok(result)
}
