//! Read-only navigation adapted from FlyCockpit's Cockpit.
//! The session owns protocol clients; servers never receive workspace/applyEdit approval.
use super::{PreparedOperation, ToolBox, ToolDefinition};
use anyhow::{Context, Result, bail};
use code_config::{Snapshot, lsp::LspServer};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex as AsyncMutex, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use url::Url;

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "lsp".into(),
        description: "Use an available language server for precise definitions, references, hover or diagnostics, especially ambiguous names. Prefer over name matching when identity matters. Input positions 1-based UTF-16; output ranges 0-based UTF-16. rename_preview is read-only; inspect with read, apply with edit. Workspace symbols need file or language. If unavailable, fall back once.".into(),
        schema: json!({"type":"object","properties":{
            "operation":{"type":"string","enum":["definition","references","hover","document_symbols","workspace_symbols","diagnostics","rename_preview"]},
            "file":{"type":"string"},"language":{"type":"string"},
            "line":{"type":"integer","minimum":1},"character":{"type":"integer","minimum":1},
            "query":{"type":"string"},"new_name":{"type":"string"}
        },"required":["operation"],"additionalProperties":false}),
    }
}

#[derive(Default)]
pub struct Manager {
    model_overrides: Mutex<HashMap<String, code_config::lsp::LspOverrides>>,
    config: Option<Arc<Mutex<Snapshot>>>,
    servers: std::collections::BTreeMap<String, LspServer>,
    state: AsyncMutex<State>,
    connected: AtomicBool,
}
#[derive(Default)]
struct State {
    closed: bool,
    clients: HashMap<String, Arc<Client>>,
}
impl Manager {
    pub(crate) fn new(
        config: Arc<Mutex<Snapshot>>,
        servers: std::collections::BTreeMap<String, LspServer>,
    ) -> Self {
        Self {
            config: Some(config),
            servers,
            ..Self::default()
        }
    }
    pub(crate) fn available(&self) -> bool {
        !self.servers.is_empty()
    }
    pub(crate) fn has_connections(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
    fn select(&self, args: &Value, file: &Path) -> Result<(String, LspServer)> {
        let language = args["language"].as_str();
        for (id, server) in &self.servers {
            let matches = if let Some(language) = language {
                language == id
            } else {
                let mut builder = globset::GlobSetBuilder::new();
                for glob in &server.file_globs {
                    builder.add(
                        globset::Glob::new(glob)
                            .with_context(|| format!("invalid LSP glob for {id}"))?,
                    );
                }
                let set = builder.build()?;
                set.is_match(file)
                    || file
                        .file_name()
                        .is_some_and(|name| set.is_match(Path::new(name)))
            };
            if matches {
                if server.command.trim().is_empty() {
                    bail!("lsp.{id}.command is empty");
                }
                return Ok((id.clone(), server.clone()));
            }
        }
        bail!(
            "No session-start LSP server matches {}; configure lsp command, args, fileGlobs and rootMarkers, then start a new session",
            file.display()
        )
    }
    async fn client(
        &self,
        language: &str,
        server: &LspServer,
        root: &Path,
        cancel: &CancellationToken,
    ) -> Result<Arc<Client>> {
        let key = format!(
            "{}\n{language}\n{}",
            root.display(),
            serde_json::to_string(server)?
        );
        let mut state = tokio::select! { _ = cancel.cancelled() => bail!("cancelled"), state = self.state.lock() => state };
        if state.closed {
            bail!("LSP session is closed");
        }
        if let Some(client) = state.clients.get(&key) {
            if !client.broken.load(Ordering::Acquire) {
                return Ok(client.clone());
            }
            let client = state.clients.remove(&key).unwrap();
            client.shutdown().await;
        }
        if state.clients.len() >= 16 {
            bail!("LSP session server limit (16) reached");
        }
        let client = Arc::new(Client::spawn(server, root).await?);
        let init = client.request("initialize", json!({
            "processId":std::process::id(),"rootUri":uri(root)?,
            "workspaceFolders":[{"uri":uri(root)?,"name":root.file_name().unwrap_or_default().to_string_lossy()}],
            "capabilities":{"general":{"positionEncodings":["utf-16"]},"textDocument":{"publishDiagnostics":{"relatedInformation":true},"diagnostic":{}},"workspace":{"applyEdit":false,"configuration":true,"workspaceFolders":true}},
            "clientInfo":{"name":"cockpit-code","version":env!("CARGO_PKG_VERSION")}
        }), cancel).await;
        match init {
            Ok(result) => {
                if result["capabilities"]["positionEncoding"]
                    .as_str()
                    .is_some_and(|e| e != "utf-16")
                {
                    client.shutdown().await;
                    bail!("LSP server selected an unsupported position encoding");
                }
                *client.capabilities.lock().unwrap() = result["capabilities"].clone();
                if let Err(error) = client.notify("initialized", json!({})).await {
                    client.shutdown().await;
                    return Err(error);
                }
            }
            Err(error) => {
                client.shutdown().await;
                return Err(error);
            }
        }
        state.clients.insert(key, client.clone());
        self.connected.store(true, Ordering::Release);
        Ok(client)
    }
    pub(crate) async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        state.closed = true;
        for (_, client) in state.clients.drain() {
            client.shutdown().await;
        }
        self.connected.store(false, Ordering::Release);
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;
#[derive(Clone)]
struct Diagnostics {
    received: tokio::time::Instant,
    version: Option<i64>,
    items: Value,
}
struct Client {
    stdin: Arc<AsyncMutex<ChildStdin>>,
    child: AsyncMutex<Child>,
    group: super::ProcessGroup,
    pending: Pending,
    next_id: std::sync::atomic::AtomicU64,
    broken: Arc<AtomicBool>,
    diagnostics: Arc<Mutex<HashMap<String, Diagnostics>>>,
    diagnostic_changed: Arc<tokio::sync::Notify>,
    capabilities: Mutex<Value>,
    documents: AsyncMutex<HashMap<PathBuf, (i64, String)>>,
    operation: AsyncMutex<()>,
    reader: Mutex<Option<JoinHandle<()>>>,
}
impl Client {
    async fn spawn(server: &LspServer, root: &Path) -> Result<Self> {
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command.process_group(0);
        let mut child = command
            .spawn()
            .with_context(|| format!("start LSP {}", server.command))?;
        let group = super::ProcessGroup(child.id().context("LSP pid unavailable")? as i32);
        let stdin = Arc::new(AsyncMutex::new(
            child.stdin.take().context("LSP stdin unavailable")?,
        ));
        let stdout = child.stdout.take().context("LSP stdout unavailable")?;
        let pending: Pending = Arc::default();
        let broken = Arc::new(AtomicBool::new(false));
        let diagnostics = Arc::new(Mutex::new(HashMap::new()));
        let diagnostic_changed = Arc::new(tokio::sync::Notify::new());
        let reader = {
            let (stdin, pending, broken, diagnostics, changed) = (
                stdin.clone(),
                pending.clone(),
                broken.clone(),
                diagnostics.clone(),
                diagnostic_changed.clone(),
            );
            let root_uri = uri(root)?;
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                while let Ok(value) = read_message(&mut reader).await {
                    if let Some(method) = value["method"].as_str() {
                        if let Some(id) = value.get("id") {
                            let response = match method {
                                "workspace/configuration" => {
                                    json!({"result":value["params"]["items"].as_array().map(|a| vec![Value::Null; a.len()]).unwrap_or_default()})
                                }
                                "workspace/workspaceFolders" => {
                                    json!({"result":[{"uri":root_uri,"name":"workspace"}]})
                                }
                                "client/registerCapability"
                                | "client/unregisterCapability"
                                | "window/workDoneProgress/create"
                                | "workspace/diagnostic/refresh"
                                | "workspace/semanticTokens/refresh"
                                | "workspace/inlayHint/refresh"
                                | "workspace/codeLens/refresh" => json!({"result":null}),
                                "workspace/applyEdit" => {
                                    json!({"result":{"applied":false,"failureReason":"Read-only LSP; apply edits through edit"}})
                                }
                                _ => {
                                    json!({"error":{"code":-32601,"message":"Unsupported client method"}})
                                }
                            };
                            let mut response = response;
                            response["jsonrpc"] = json!("2.0");
                            response["id"] = id.clone();
                            if write_message(&stdin, &response).await.is_err() {
                                break;
                            }
                        } else if method == "textDocument/publishDiagnostics"
                            && let Some(uri) = value["params"]["uri"].as_str()
                        {
                            let mut items = diagnostics.lock().unwrap();
                            if items.len() < 512 || items.contains_key(uri) {
                                items.insert(
                                    uri.to_owned(),
                                    Diagnostics {
                                        received: tokio::time::Instant::now(),
                                        version: value["params"]["version"].as_i64(),
                                        items: value["params"]["diagnostics"].clone(),
                                    },
                                );
                                changed.notify_waiters();
                            }
                        }
                    } else if let Some(id) = value["id"].as_u64()
                        && let Some(tx) = pending.lock().unwrap().remove(&id)
                    {
                        let _ = tx.send(value);
                    }
                }
                broken.store(true, Ordering::Release);
                pending.lock().unwrap().clear();
                changed.notify_waiters();
            })
        };
        Ok(Self {
            stdin,
            child: AsyncMutex::new(child),
            group,
            pending,
            next_id: 1.into(),
            broken,
            diagnostics,
            diagnostic_changed,
            capabilities: Mutex::new(Value::Null),
            documents: AsyncMutex::default(),
            operation: AsyncMutex::default(),
            reader: Mutex::new(Some(reader)),
        })
    }
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        tokio::time::timeout(
            Duration::from_secs(5),
            write_message(
                &self.stdin,
                &json!({"jsonrpc":"2.0","method":method,"params":params}),
            ),
        )
        .await
        .context("LSP write timed out")?
    }
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
            result = tokio::time::timeout(Duration::from_secs(30), async {
                write_message(&self.stdin, &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
                let response = rx.await.context("LSP server disconnected")?;
                if let Some(error) = response.get("error") { bail!("LSP {method}: {error}"); }
                Ok(response["result"].clone())
            }) => result.unwrap_or_else(|_| Err(anyhow::anyhow!("LSP request timed out"))),
        };
        self.pending.lock().unwrap().remove(&id);
        if result.is_err() {
            let _ = self.notify("$/cancelRequest", json!({"id":id})).await;
        }
        result
    }
    async fn open(&self, file: &Path, language: &str, text: &str) -> Result<i64> {
        let mut docs = self.documents.lock().await;
        if let Some((version, previous)) = docs.get(file)
            && previous == text
        {
            return Ok(*version);
        }
        let version = docs.get(file).map_or(1, |(v, _)| v + 1);
        self.diagnostics.lock().unwrap().remove(&uri(file)?);
        if version == 1 {
            self.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri(file)?,"languageId":language,"version":version,"text":text}})).await?;
        } else {
            self.notify("textDocument/didChange", json!({"textDocument":{"uri":uri(file)?,"version":version},"contentChanges":[{"text":text}]})).await?;
        }
        docs.insert(file.into(), (version, text.into()));
        Ok(version)
    }
    async fn shutdown(&self) {
        let cancel = CancellationToken::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            self.request("shutdown", Value::Null, &cancel),
        )
        .await;
        let _ = self.notify("exit", Value::Null).await;
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(1), child.wait())
            .await
            .is_err()
        {
            self.group.signal(libc::SIGKILL);
            let _ = child.wait().await;
        }
        // Kill descendants even when the server exited successfully.
        self.group.signal(libc::SIGKILL);
        let reader = self.reader.lock().unwrap().take();
        if let Some(reader) = reader {
            reader.abort();
            let _ = reader.await;
        }
        self.pending.lock().unwrap().clear();
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.group.signal(libc::SIGKILL);
        if let Some(reader) = self.reader.lock().unwrap().take() {
            reader.abort();
        }
    }
}
async fn write_message(stdin: &AsyncMutex<ChildStdin>, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}
async fn read_message(reader: &mut BufReader<tokio::process::ChildStdout>) -> Result<Value> {
    let mut length = None;
    let mut headers = 0;
    loop {
        let mut line = String::new();
        let n = (&mut *reader).take(8193).read_line(&mut line).await?;
        if n == 0 {
            bail!("LSP EOF");
        }
        headers += n;
        if headers > 8192 {
            bail!("LSP headers too large");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("LSP Content-Length missing")?;
    if length > 8 * 1024 * 1024 {
        bail!("LSP message too large");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}
fn uri(path: &Path) -> Result<String> {
    Ok(Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("invalid LSP file path"))?
        .into())
}
fn root(file: &Path, cwd: &Path, markers: &[String]) -> PathBuf {
    let parent = if file.is_dir() {
        file
    } else {
        file.parent().unwrap_or(cwd)
    };
    for dir in parent.ancestors() {
        if markers.iter().any(|marker| dir.join(marker).exists()) {
            return dir.into();
        }
        if dir == cwd {
            break;
        }
    }
    if file.starts_with(cwd) {
        cwd.into()
    } else {
        parent.into()
    }
}
fn result_paths(value: &Value, paths: &mut BTreeSet<PathBuf>) {
    match value {
        Value::String(s) if s.starts_with("file:") => {
            if let Ok(url) = Url::parse(s)
                && let Ok(path) = url.to_file_path()
            {
                paths.insert(path);
            }
        }
        Value::Array(items) => {
            for item in items {
                result_paths(item, paths);
            }
        }
        Value::Object(items) => {
            for (key, item) in items {
                result_paths(&Value::String(key.clone()), paths);
                result_paths(item, paths);
            }
        }
        _ => {}
    }
}
impl ToolBox {
    pub(crate) async fn authorize_read_path(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let path = self
            .resolver
            .resolve(path.to_str().context("path is not UTF-8")?)?;
        let call = crate::schema::ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: "read".into(),
            arguments: json!({"path":path}),
            complete: true,
        };
        let mut op = PreparedOperation::metadata(call, self.cwd.clone(), Some(path.clone()));
        op.agent_id = self.agent_id.clone();
        op.breadcrumb = self.breadcrumb.clone();
        op.job_id = self.job_id.clone();
        self.gate.authorize(&op, cancel).await?;
        self.resolver.validate(&path)
    }
    pub(super) async fn lsp(&self, args: &Value, cancel: &CancellationToken) -> Result<String> {
        let operation = args["operation"]
            .as_str()
            .context("operation is required")?;
        if operation != "workspace_symbols" && args["file"].as_str().is_none() {
            bail!("file is required for {operation}");
        }
        let file = self
            .resolver
            .resolve(args["file"].as_str().unwrap_or("."))?;
        let (language, server) = self.lsp.select(args, &file)?;
        let root = root(&file, &self.cwd, &server.root_markers);
        if !root.starts_with(&self.cwd) {
            self.authorize_read_path(&root, cancel).await?;
        }
        self.resolver.validate(&file)?;
        let text = if operation == "workspace_symbols" {
            None
        } else {
            let path = file.clone();
            Some(
                tokio::task::spawn_blocking(move || -> Result<String> {
                    Ok(String::from_utf8(super::read_bytes(&path)?)?)
                })
                .await??,
            )
        };
        let position = if matches!(
            operation,
            "hover" | "definition" | "references" | "rename_preview"
        ) {
            let number = |key: &str| -> Result<u64> {
                let n = args[key].as_u64().unwrap_or(1);
                if n == 0 || n > u32::MAX as u64 {
                    bail!("{key} must be between 1 and {}", u32::MAX);
                }
                Ok(n - 1)
            };
            let line = number("line")?;
            let character = number("character")?;
            let line_text = text
                .as_deref()
                .unwrap_or("")
                .split('\n')
                .nth(line as usize)
                .context("line is outside the file")?;
            if character as usize > line_text.trim_end_matches('\r').encode_utf16().count() {
                bail!("character is outside the line (UTF-16 units)");
            }
            json!({"line":line,"character":character})
        } else {
            Value::Null
        };
        let client = self.lsp.client(&language, &server, &root, cancel).await?;
        let _serial = tokio::select! { _ = cancel.cancelled() => bail!("cancelled"), serial = client.operation.lock() => serial };
        let version = if let Some(text) = text {
            client.open(&file, &language, &text).await?
        } else {
            0
        };
        let mut params = json!({"textDocument":{"uri":uri(&file)?},"position":position});
        let method = match operation {
            "definition" => "textDocument/definition",
            "references" => {
                params["context"] = json!({"includeDeclaration":true});
                "textDocument/references"
            }
            "hover" => "textDocument/hover",
            "document_symbols" => {
                params = json!({"textDocument":{"uri":uri(&file)?}});
                "textDocument/documentSymbol"
            }
            "workspace_symbols" => {
                params = json!({"query":args["query"].as_str().unwrap_or("")});
                "workspace/symbol"
            }
            "rename_preview" => {
                params["newName"] = json!(
                    args["new_name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .context("new_name is required")?
                );
                "textDocument/rename"
            }
            "diagnostics" => "textDocument/diagnostic",
            _ => bail!("unsupported LSP operation {operation}"),
        };
        let pull = client
            .capabilities
            .lock()
            .unwrap()
            .get("diagnosticProvider")
            .is_some_and(|v| !v.is_null() && v != &Value::Bool(false));
        let result = if operation == "diagnostics" && !pull {
            let document_uri = uri(&file)?;
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let changed = client.diagnostic_changed.notified(); tokio::pin!(changed); changed.as_mut().enable();
                    if let Some(diag) = client.diagnostics.lock().unwrap().get(&document_uri).cloned() && diag.version.is_none_or(|v| v == version) { break Ok(json!({"items":diag.items,"version":diag.version,"source":"publishDiagnostics"})); }
                    if client.broken.load(Ordering::Acquire) { bail!("LSP server disconnected"); }
                    tokio::select! { _ = cancel.cancelled() => bail!("cancelled"), _ = changed => {} }
                }
            }).await.unwrap_or_else(|_| Ok(json!({"items":null,"status":"pending","message":"No diagnostics published for this document yet; retry."})))?
        } else {
            if operation == "diagnostics" {
                params = json!({"textDocument":{"uri":uri(&file)?}});
            }
            client.request(method, params, cancel).await?
        };
        let mut paths = BTreeSet::new();
        result_paths(&result, &mut paths);
        for path in paths {
            let resolved = self
                .resolver
                .resolve(path.to_str().context("LSP path is not UTF-8")?)?;
            if !resolved.starts_with(&self.cwd) && resolved != file {
                self.authorize_read_path(&resolved, cancel).await?;
            }
        }
        Ok(json!({"operation":operation,"language":language,"root":root,"readOnly":true,"rangeEncoding":"0-based UTF-16","result":result}).to_string())
    }
}

pub(crate) struct WriteBaseline {
    client: Arc<Client>,
    language: String,
    before: Vec<Value>,
    mode: code_config::lsp::AfterWrite,
    wait: Duration,
}
impl Manager {
    pub(crate) fn set_model_override(
        &self,
        agent: &str,
        value: Option<code_config::lsp::LspOverrides>,
    ) {
        let mut overrides = self.model_overrides.lock().unwrap();
        if let Some(value) = value {
            overrides.insert(agent.into(), value);
        } else {
            overrides.remove(agent);
        }
    }
    fn write_settings(&self, agent: &str) -> Option<(code_config::lsp::AfterWrite, Duration)> {
        let config = self.config.as_ref()?.lock().unwrap();
        let overrides = self.model_overrides.lock().unwrap();
        let model = overrides.get(agent);
        let mode = model
            .and_then(|m| m.after_write)
            .unwrap_or(config.config.lsp.after_write);
        let wait = model
            .and_then(|m| m.after_write_ms)
            .unwrap_or(config.config.lsp.after_write_ms);
        (mode != code_config::lsp::AfterWrite::Off && wait > 0)
            .then_some((mode, Duration::from_millis(wait)))
    }
    /// Baseline reads happen after authorization; any LSP failure is advisory only.
    pub(crate) async fn before_write(
        &self,
        cwd: &Path,
        file: &Path,
        agent: &str,
        cancel: &CancellationToken,
    ) -> Option<WriteBaseline> {
        let (mode, wait) = self.write_settings(agent)?;
        let (language, server) = self.select(&json!({}), file).ok()?;
        let root = root(file, cwd, &server.root_markers);
        tokio::time::timeout(wait,async {
            let client=self.client(&language,&server,&root,cancel).await.ok()?;
            let _serial=tokio::select!{_=cancel.cancelled()=>return None,guard=client.operation.lock()=>guard};
            let existing=file.exists();
            let text=if existing{std::fs::read_to_string(file).ok()?}else{String::new()};
            let version=client.open(file,&language,&text).await.ok()?;
            let before=if existing{client.settled_diagnostics(file,version,cancel).await.ok()?}else{Vec::new()};
            drop(_serial);
            Some(WriteBaseline{client,language,before,mode,wait})
        }).await.ok().flatten()
    }
    pub(crate) async fn after_write(
        &self,
        before: Option<WriteBaseline>,
        file: &Path,
        text: &str,
        cancel: &CancellationToken,
    ) -> Option<String> {
        let before = before?;
        tokio::time::timeout(before.wait,async {
            if std::fs::read_to_string(file).ok().as_deref()!=Some(text){return None;}
            let _serial=tokio::select!{_=cancel.cancelled()=>return None,guard=before.client.operation.lock()=>guard};
            let version=before.client.open(file,&before.language,text).await.ok()?;
            before.client.notify("textDocument/didSave",json!({"textDocument":{"uri":uri(file).ok()?},"text":text})).await.ok()?;
            let after=before.client.settled_diagnostics(file,version,cancel).await.ok()?;
            if std::fs::read_to_string(file).ok().as_deref()!=Some(text){return None;}
            Some(render_introduced(file,&before.before,after,before.mode))
        }).await.ok().flatten()
    }
}
impl Client {
    async fn settled_diagnostics(
        &self,
        file: &Path,
        version: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<Value>> {
        let document = uri(file)?;
        if self
            .capabilities
            .lock()
            .unwrap()
            .get("diagnosticProvider")
            .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
        {
            let result = self
                .request(
                    "textDocument/diagnostic",
                    json!({"textDocument":{"uri":document}}),
                    cancel,
                )
                .await?;
            return result["items"]
                .as_array()
                .cloned()
                .context("no document diagnostic result");
        }
        loop {
            let changed = self.diagnostic_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let current = self
                .diagnostics
                .lock()
                .unwrap()
                .get(&document)
                .cloned()
                .filter(|d| d.version.is_none_or(|v| v == version));
            if let Some(diag) = current {
                let settled = diag.received + Duration::from_millis(100);
                tokio::select! {_=cancel.cancelled()=>bail!("cancelled"),_=&mut changed=>continue,_=tokio::time::sleep_until(settled)=>return diag.items.as_array().cloned().context("invalid published diagnostics")}
            } else {
                if self.broken.load(Ordering::Acquire) {
                    bail!("LSP disconnected");
                }
                tokio::select! {_=cancel.cancelled()=>bail!("cancelled"),_=&mut changed=>{}}
            }
        }
    }
}
fn render_introduced(
    file: &Path,
    before: &[Value],
    after: Vec<Value>,
    mode: code_config::lsp::AfterWrite,
) -> String {
    let relevant = |v: &Value| {
        v["severity"].as_u64().unwrap_or(1) == 1
            || (mode == code_config::lsp::AfterWrite::ErrorsWarnings && v["severity"] == 2)
    };
    let key = |v: &Value| json!([v["severity"], v["source"], v["code"], v["message"]]).to_string();
    let mut old = std::collections::BTreeMap::<String, usize>::new();
    for diag in before.iter().filter(|d| relevant(d)) {
        *old.entry(key(diag)).or_default() += 1;
    }
    let mut introduced = Vec::new();
    let mut existing = 0;
    for diag in after.into_iter().filter(|d| relevant(d)) {
        let count = old.entry(key(&diag)).or_default();
        if *count > 0 {
            *count -= 1;
            existing += 1;
        } else {
            introduced.push(diag);
        }
    }
    introduced.sort_by_key(|d| {
        (
            d["severity"].as_u64().unwrap_or(1),
            d["range"]["start"]["line"].as_u64().unwrap_or(0),
        )
    });
    let errors = introduced
        .iter()
        .filter(|d| d["severity"].as_u64().unwrap_or(1) == 1)
        .count();
    let warnings = introduced.len() - errors;
    if introduced.is_empty() && existing == 0 {
        return String::new();
    }
    let mut out = format!(
        "\nLSP: +{errors} errors, +{warnings} warnings introduced ({existing} pre-existing)"
    );
    for d in introduced.iter().take(19) {
        let message = d["message"]
            .as_str()
            .unwrap_or("diagnostic")
            .replace(['\r', '\n'], " ")
            .chars()
            .take(300)
            .collect::<String>();
        out.push_str(&format!(
            "\n{}:{}:{}: {}: {message}",
            file.display(),
            d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1,
            d["range"]["start"]["character"].as_u64().unwrap_or(0) + 1,
            if d["severity"] == 2 {
                "warning"
            } else {
                "error"
            }
        ));
    }
    out
}
