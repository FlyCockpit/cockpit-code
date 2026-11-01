//! Cockpit's legacy SSE transport, using the engine's bounded incremental frame parser.
use super::timeout::{McpTimeouts, timeout_error};
use crate::mcp::protocol::*;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

pub struct SseClient {
    endpoint: String,
    headers: BTreeMap<String, String>,
    http: reqwest::Client,
    next_id: u64,
    post_url: Option<String>,
    rx: Option<mpsc::Receiver<Value>>,
    pump: Option<tokio::task::JoinHandle<()>>,
}
impl SseClient {
    pub fn new(
        endpoint: String,
        headers: BTreeMap<String, String>,
        timeouts: McpTimeouts,
    ) -> Result<Self> {
        Ok(Self {
            http: super::timeout::client(timeouts, &endpoint)?,
            endpoint,
            headers,
            next_id: 1,
            post_url: None,
            rx: None,
            pump: None,
        })
    }
    async fn connect(&mut self) -> Result<()> {
        let mut builder = self
            .http
            .get(&self.endpoint)
            .header("Accept", "text/event-stream");
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| timeout_error("MCP SSE connect", e))?;
        if !response.status().is_success() {
            bail!("MCP SSE HTTP {}", response.status());
        }
        let mut stream = response.bytes_stream();
        let mut parser = crate::model::sse::Parser::default();
        let mut pending = vec![];
        while self.post_url.is_none() {
            let chunk = stream
                .next()
                .await
                .context("MCP SSE ended before endpoint")??;
            for data in parser.push(&chunk)? {
                if self.post_url.is_none() {
                    let base = super::timeout::validate_remote_endpoint(&self.endpoint)?;
                    let target = base.join(&data).context("invalid MCP SSE endpoint")?;
                    if !super::timeout::same_origin(&base, &target) {
                        bail!("MCP SSE endpoint changed origin");
                    }
                    self.post_url = Some(target.into());
                } else if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    pending.push(value);
                }
            }
        }
        let (tx, rx) = mpsc::channel(128);
        self.pump = Some(tokio::spawn(async move {
            for value in pending {
                if tx.send(value).await.is_err() {
                    return;
                }
            }
            while let Some(Ok(chunk)) = stream.next().await {
                let Ok(frames) = parser.push(&chunk) else {
                    return;
                };
                for data in frames {
                    if let Ok(value) = serde_json::from_str(&data)
                        && tx.send(value).await.is_err()
                    {
                        return;
                    }
                }
            }
        }));
        self.rx = Some(rx);
        Ok(())
    }
    async fn post(&self, value: Value) -> Result<()> {
        let mut builder = self
            .http
            .post(self.post_url.as_ref().context("MCP SSE not connected")?)
            .json(&value);
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| timeout_error("MCP SSE POST", e))?;
        if !response.status().is_success() {
            bail!("MCP SSE POST HTTP {}", response.status());
        }
        Ok(())
    }
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        if self.post_url.is_none() {
            self.connect().await?;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.post(serde_json::to_value(JsonRpcRequest::new(
            id, method, params,
        ))?)
        .await?;
        let rx = self.rx.as_mut().context("MCP SSE receiver missing")?;
        while let Some(value) = rx.recv().await {
            if value["id"].as_u64() == Some(id) {
                return serde_json::from_value::<JsonRpcResponse>(value)?.into_result();
            }
        }
        bail!("MCP SSE stream ended before response")
    }
}
impl Drop for SseClient {
    fn drop(&mut self) {
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
    }
}
#[async_trait]
impl McpClient for SseClient {
    async fn shutdown(&mut self) {
        if let Some(task) = self.pump.take() {
            task.abort();
            let _ = task.await;
        }
        self.rx = None;
        self.post_url = None;
    }
    async fn initialize(&mut self) -> Result<()> {
        self.request("initialize", Some(initialize_params()))
            .await?;
        self.post(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await
    }
    async fn list_tools(&mut self) -> Result<Vec<ToolDescriptor>> {
        let mut tools = vec![];
        let mut params = None;
        let mut cursors = std::collections::BTreeSet::new();
        loop {
            let value = self.request("tools/list", params).await?;
            tools.extend(parse_tools_list(&value)?);
            let Some(cursor) = value["nextCursor"].as_str() else {
                return Ok(tools);
            };
            if !cursors.insert(cursor.to_owned()) || cursors.len() > 1000 {
                bail!("MCP tools/list cursor loop or too many pages");
            }
            params = Some(json!({"cursor":cursor}));
        }
    }
    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        self.request("tools/call", Some(json!({"name":name,"arguments":args})))
            .await
    }
}
