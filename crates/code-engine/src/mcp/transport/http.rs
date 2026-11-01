//! Cockpit's Streamable HTTP transport, with session headers and incremental SSE.
use super::super::protocol::*;
use super::timeout::{McpTimeouts, timeout_error};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct HttpClient {
    endpoint: String,
    headers: BTreeMap<String, String>,
    http: reqwest::Client,
    next_id: u64,
    session_id: Option<String>,
    protocol_version: String,
}
impl HttpClient {
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
            session_id: None,
            protocol_version: "2025-03-26".into(),
        })
    }
    fn builder(&self) -> reqwest::RequestBuilder {
        let mut builder = self
            .http
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        if let Some(id) = &self.session_id {
            builder = builder.header("Mcp-Session-Id", id);
        }
        builder.header("MCP-Protocol-Version", &self.protocol_version)
    }
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .builder()
            .json(&JsonRpcRequest::new(id, method, params))
            .send()
            .await
            .map_err(|e| timeout_error("MCP HTTP request", e))?;
        if !response.status().is_success() {
            bail!("MCP HTTP {}", response.status());
        }
        if let Some(id) = response.headers().get("mcp-session-id") {
            self.session_id = Some(id.to_str()?.into());
        }
        let sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.contains("text/event-stream"));
        let mut stream = response.bytes_stream();
        let mut parser = crate::model::sse::Parser::default();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if sse {
                for frame in parser.push(&chunk)? {
                    let value: Value = serde_json::from_str(&frame)?;
                    if value["id"].as_u64() == Some(id) {
                        return serde_json::from_value::<JsonRpcResponse>(value)?.into_result();
                    }
                }
            } else {
                if body.len() + chunk.len() > 16 * 1024 * 1024 {
                    bail!("MCP HTTP response exceeds 16 MiB");
                }
                body.extend_from_slice(&chunk);
            }
        }
        let value: Value =
            serde_json::from_slice(&body).context("MCP HTTP response missing JSON-RPC message")?;
        if value["id"].as_u64() != Some(id) {
            bail!("MCP HTTP response id mismatch");
        }
        serde_json::from_value::<JsonRpcResponse>(value)?.into_result()
    }
}
#[async_trait]
impl McpClient for HttpClient {
    async fn initialize(&mut self) -> Result<()> {
        let value = self
            .request("initialize", Some(initialize_params()))
            .await?;
        if let Some(version) = value["protocolVersion"].as_str() {
            self.protocol_version = version.into();
        }
        let response = self
            .builder()
            .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("MCP initialized notification HTTP {}", response.status());
        }
        Ok(())
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
