//! Session-scoped lazy MCP connections. Remote tools never enter the model's tools array.
pub mod protocol;
mod transport;
use anyhow::{Result, bail};
use code_config::{Mcp, Snapshot, Transport};
use protocol::{McpClient, ToolDescriptor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    /// disconnected, connecting, connected, or error.
    pub connection_state: String,
    pub tool_count: usize,
    pub last_error: Option<String>,
}
struct Connection {
    client: Option<Box<dyn McpClient>>,
    tools: Vec<ToolDescriptor>,
    revision: u64,
}
struct Entry {
    config: Mutex<Mcp>,
    revision: AtomicU64,
    configured: AtomicBool,
    status: Mutex<McpStatus>,
    connection: AsyncMutex<Connection>,
    epoch: Mutex<CancellationToken>,
}
pub(crate) struct Manager {
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
    cwd: PathBuf,
    closing: AtomicBool,
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    notices: Mutex<VecDeque<String>>,
}
impl Manager {
    pub fn new(snapshot: &Snapshot, enabled: &BTreeSet<String>, cwd: PathBuf) -> Self {
        let manager = Self {
            entries: Mutex::default(),
            cwd,
            closing: AtomicBool::new(false),
            workers: Mutex::default(),
            notices: Mutex::default(),
        };
        manager.configure(snapshot, enabled);
        manager
    }
    pub fn configure(&self, snapshot: &Snapshot, enabled: &BTreeSet<String>) {
        let mut entries = self.entries.lock().unwrap();
        for entry in entries.values() {
            entry.configured.store(false, Ordering::Release);
        }
        for (name, config) in &snapshot.mcps {
            let entry = entries.entry(name.clone()).or_insert_with(|| {
                Arc::new(Entry {
                    config: Mutex::new(config.clone()),
                    revision: AtomicU64::new(0),
                    configured: AtomicBool::new(true),
                    status: Mutex::new(McpStatus {
                        name: name.clone(),
                        description: config.description.clone(),
                        enabled: enabled.contains(name),
                        connection_state: "disconnected".into(),
                        tool_count: 0,
                        last_error: None,
                    }),
                    connection: AsyncMutex::new(Connection {
                        client: None,
                        tools: vec![],
                        revision: 0,
                    }),
                    epoch: Mutex::new(CancellationToken::new()),
                })
            });
            entry.configured.store(true, Ordering::Release);
            let mut old = entry.config.lock().unwrap();
            if serde_json::to_value(&*old).ok() != serde_json::to_value(config).ok() {
                entry.revision.fetch_add(1, Ordering::AcqRel);
                let mut epoch = entry.epoch.lock().unwrap();
                epoch.cancel();
                *epoch = CancellationToken::new();
            }
            *old = config.clone();
            entry.status.lock().unwrap().description = config.description.clone();
        }
    }
    pub fn refresh(self: &Arc<Self>, snapshot: &Snapshot, enabled: &BTreeSet<String>) {
        self.configure(snapshot, enabled);
        self.set_enabled(enabled);
    }
    fn entry(&self, name: &str) -> Result<Arc<Entry>> {
        self.entries
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("MCP server {name} is not configured"))
    }
    /// Never wait behind an in-flight call merely to prepare a model request.
    pub fn poll_health(&self) {
        let entries = self
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            if !entry.configured.load(Ordering::Acquire) {
                continue;
            }
            let Ok(mut connection) = entry.connection.try_lock() else {
                continue;
            };
            let mut status = entry.status.lock().unwrap();
            if !status.enabled || status.connection_state != "connected" {
                continue;
            }
            if connection
                .client
                .as_mut()
                .is_some_and(|client| client.check_health().is_err())
            {
                status.connection_state = "error".into();
                status.tool_count = 0;
                status.last_error = Some("MCP server process exited".into());
                self.notices.lock().unwrap().push_back(format!("MCP server {} crashed. Its tools are unavailable until the next call reconnects.", status.name));
            }
        }
    }
    pub fn take_notices(&self) -> Vec<String> {
        self.notices.lock().unwrap().drain(..).collect()
    }
    pub fn statuses(&self) -> Vec<McpStatus> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.configured.load(Ordering::Acquire))
            .map(|e| e.status.lock().unwrap().clone())
            .collect()
    }
    pub fn has_connections(&self) -> bool {
        self.statuses()
            .iter()
            .any(|s| s.connection_state != "disconnected")
            || self
                .workers
                .lock()
                .unwrap()
                .iter()
                .any(|task| !task.is_finished())
    }
    pub fn set_enabled(self: &Arc<Self>, enabled: &BTreeSet<String>) {
        let entries = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|(n, e)| (n.clone(), e.clone()))
            .collect::<Vec<_>>();
        for (name, entry) in entries {
            let on = enabled.contains(&name) && entry.configured.load(Ordering::Acquire);
            let previous = {
                let mut status = entry.status.lock().unwrap();
                let previous = status.enabled;
                status.enabled = on;
                previous
            };
            if previous == on {
                continue;
            }
            self.notices.lock().unwrap().push_back(format!("MCP server {name} {}. {}", if on { "enabled" } else { "disabled" }, if on { "Its tool catalog is being connected; tools will be listed in the connection notice and are available through script as mcp.<server>.<tool>." } else { "Its tools are unavailable through script." }));
            {
                let mut epoch = entry.epoch.lock().unwrap();
                epoch.cancel();
                *epoch = CancellationToken::new();
            }
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let manager = self.clone();
                let task = runtime.spawn(async move {
                    if on {
                        let _ = manager.catalog(&name, &CancellationToken::new()).await;
                    } else {
                        let mut connection = entry.connection.lock().await;
                        if entry.status.lock().unwrap().enabled {
                            return;
                        }
                        if let Some(mut client) = connection.client.take() {
                            client.shutdown().await;
                        }
                        connection.tools.clear();
                        let mut status = entry.status.lock().unwrap();
                        status.connection_state = "disconnected".into();
                        status.tool_count = 0;
                    }
                });
                let mut workers = self.workers.lock().unwrap();
                workers.retain(|t| !t.is_finished());
                workers.push(task);
            }
        }
    }
    async fn connect(&self, entry: &Entry, connection: &mut Connection) -> Result<()> {
        if connection.client.is_some() {
            return Ok(());
        }
        entry.status.lock().unwrap().connection_state = "connecting".into();
        let config = entry.config.lock().unwrap().clone();
        let resolved = config
            .connection()
            .ok_or_else(|| anyhow::anyhow!("MCP connection not resolved"))?;
        let timeouts = transport::timeout::McpTimeouts::from_secs(
            resolved.timeouts.startup_secs,
            resolved.timeouts.call_secs,
        );
        let client: Box<dyn McpClient> = match resolved.transport {
            Transport::Stdio => Box::new(transport::stdio::StdioClient::spawn(
                resolved
                    .command
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("MCP command required"))?,
                &resolved.args,
                &resolved.env,
                &self.cwd,
            )?),
            Transport::Http => Box::new(transport::http::HttpClient::new(
                resolved
                    .url
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("MCP URL required"))?,
                resolved.headers.clone(),
                timeouts,
            )?),
            Transport::Sse => Box::new(transport::sse::SseClient::new(
                resolved
                    .url
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("MCP URL required"))?,
                resolved.headers.clone(),
                timeouts,
            )?),
        };
        // Publish before await so cancellation/error cleanup can always reap the child.
        connection.client = Some(client);
        let startup = async {
            let client = connection.client.as_mut().unwrap();
            client.initialize().await?;
            let tools = client.list_tools().await?;
            connection.tools = tools;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(
            Duration::from_secs(resolved.timeouts.startup_secs.max(1)),
            startup,
        )
        .await
        .map_err(|_| anyhow::anyhow!("MCP startup timed out"))??;
        let mut status = entry.status.lock().unwrap();
        status.connection_state = "connected".into();
        status.tool_count = connection.tools.len();
        status.last_error = None;
        let names = connection
            .tools
            .iter()
            .map(|t| format!("mcp.{}.{}", status.name, t.name))
            .collect::<Vec<_>>();
        self.notices.lock().unwrap().push_back(format!("MCP server {} connected or recovered. Tools available through script: {}. Discover schemas with tools.describe(name=...).", status.name, serde_json::json!(names)));
        Ok(())
    }
    async fn operate(
        &self,
        name: &str,
        call: Option<(&str, Value)>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let entry = self.entry(name)?;
        let epoch = entry.epoch.lock().unwrap().clone();
        let mut connection = tokio::select! { _=cancel.cancelled()=>bail!("MCP call cancelled"), _=epoch.cancelled()=>bail!("MCP enable state changed"), c=entry.connection.lock()=>c };
        if self.closing.load(Ordering::Acquire)
            || !entry.configured.load(Ordering::Acquire)
            || !entry.status.lock().unwrap().enabled
        {
            bail!("MCP server {name} is disabled or shutting down");
        }
        let revision = entry.revision.load(Ordering::Acquire);
        if connection.revision != revision {
            if let Some(mut client) = connection.client.take() {
                client.shutdown().await;
            }
            connection.tools.clear();
            connection.revision = revision;
        }
        if entry.status.lock().unwrap().connection_state == "error" {
            if let Some(mut client) = connection.client.take() {
                client.shutdown().await;
            }
            connection.tools.clear();
        }
        let seconds = entry.config.lock().unwrap().timeouts.call_secs;
        let operation = async {
            self.connect(&entry, &mut connection).await?;
            if let Some((tool, args)) = call {
                if !connection.tools.iter().any(|t| t.name == tool) {
                    bail!("MCP tool {name}.{tool} not found; use tools.search/describe");
                }
                tokio::time::timeout(
                    Duration::from_secs(seconds.max(1)),
                    connection.client.as_mut().unwrap().call_tool(tool, args),
                )
                .await
                .map_err(|_| anyhow::anyhow!("MCP call timed out"))?
            } else {
                Ok(serde_json::to_value(&connection.tools)?)
            }
        };
        let result = tokio::select! { biased; _=cancel.cancelled()=>Err(anyhow::anyhow!("MCP call cancelled")), _=epoch.cancelled()=>Err(anyhow::anyhow!("MCP enable state changed")), r=operation=>r };
        if let Err(error) = &result {
            if let Some(mut client) = connection.client.take() {
                client.shutdown().await;
            }
            connection.tools.clear();
            // reqwest errors may contain credential-bearing URLs; keep only their category.
            let message = if error.chain().any(|e| e.is::<reqwest::Error>()) {
                "MCP network request failed".into()
            } else {
                error.to_string()
            };
            let config = entry.config.lock().unwrap();
            let mut message = message;
            if let Some(resolved) = config.connection() {
                if let Some(url) = &resolved.url {
                    message = message.replace(url, "<configured endpoint>");
                }
                for value in resolved.env.values().chain(resolved.headers.values()) {
                    if !value.is_empty() {
                        message = message.replace(value, "<configured value>");
                    }
                }
            }
            let mut status = entry.status.lock().unwrap();
            status.connection_state = "error".into();
            status.tool_count = 0;
            status.last_error = Some(message.clone());
            self.notices.lock().unwrap().push_back(format!("MCP server {name} became unavailable: {message}. Its tools are unavailable until recovery; the next call reconnects."));
            return Err(anyhow::anyhow!(
                "MCP {name}: {message}; next call reconnects"
            ));
        }
        result
    }
    pub async fn catalog(
        &self,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<ToolDescriptor>> {
        Ok(serde_json::from_value(
            self.operate(name, None, cancel).await?,
        )?)
    }
    pub async fn call(
        &self,
        name: &str,
        tool: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.operate(name, Some((tool, args)), cancel).await
    }
    pub async fn shutdown(&self) {
        self.closing.store(true, Ordering::Release);
        let entries = self
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in &entries {
            entry.epoch.lock().unwrap().cancel();
        }
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        for task in workers {
            let _ = task.await;
        }
        for entry in entries {
            let mut c = entry.connection.lock().await;
            if let Some(mut client) = c.client.take() {
                client.shutdown().await;
            }
            c.tools.clear();
            let mut s = entry.status.lock().unwrap();
            s.connection_state = "disconnected".into();
            s.tool_count = 0;
        }
    }
}
