//! Adapted from Cockpit's bounded line-delimited JSON-RPC transport.
use super::super::protocol::*;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};

pub struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    process_group: Option<u32>,
}
impl StdioClient {
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().context("MCP command could not start")?;
        let process_group = child.id();
        Ok(Self {
            process_group,
            stdin: child.stdin.take().context("MCP stdin missing")?,
            stdout: BufReader::new(child.stdout.take().context("MCP stdout missing")?),
            child,
            next_id: 1,
        })
    }
    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.process_group.take() {
            // This process group belongs to this live connection; no persisted PID is used.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
    fn has_exited(&mut self) -> Result<bool> {
        #[cfg(unix)]
        {
            let Some(pid) = self.child.id() else {
                return Ok(true);
            };
            // Observe exit without reaping: the waitable leader reserves its
            // PID/PGID until we have killed any surviving group members.
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                return Err(std::io::Error::last_os_error().into());
            }
            if unsafe { info.si_pid() } == 0 {
                return Ok(false);
            }
            self.kill_group();
            let _ = self.child.try_wait()?;
            Ok(true)
        }
        #[cfg(not(unix))]
        {
            Ok(self.child.try_wait()?.is_some())
        }
    }
    async fn notification(&mut self, method: &str) -> Result<()> {
        let mut bytes = serde_json::to_vec(&json!({"jsonrpc":"2.0","method":method}))?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<JsonRpcResponse> {
        if self.has_exited()? {
            bail!("MCP server exited; next call will restart it");
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut bytes = serde_json::to_vec(&JsonRpcRequest::new(id, method, params))?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        loop {
            let mut line = Vec::new();
            let count = (&mut self.stdout)
                .take(16 * 1024 * 1024 + 1)
                .read_until(b'\n', &mut line)
                .await?;
            if count == 0 {
                bail!("MCP server closed stdout; next call will restart it");
            }
            if line.len() > 16 * 1024 * 1024 {
                bail!("MCP response exceeds 16 MiB");
            }
            let value: Value =
                serde_json::from_slice(&line).context("invalid MCP JSON-RPC response")?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(serde_json::from_value(value)?);
            }
            if value.get("method").is_some() && value.get("id").is_some() {
                let mut response = serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":value["id"],"error":{"code":-32601,"message":"client method not supported"}}),
                )?;
                response.push(b'\n');
                self.stdin.write_all(&response).await?;
            }
        }
    }
}
impl Drop for StdioClient {
    fn drop(&mut self) {
        self.kill_group();
    }
}
#[async_trait]
impl McpClient for StdioClient {
    fn check_health(&mut self) -> Result<()> {
        if self.has_exited()? {
            bail!("MCP server process exited; next call reconnects");
        }
        Ok(())
    }
    async fn initialize(&mut self) -> Result<()> {
        self.request("initialize", Some(initialize_params()))
            .await?
            .into_result()?;
        self.notification("notifications/initialized").await
    }
    async fn shutdown(&mut self) {
        self.kill_group();
        let _ = self.child.wait().await;
    }
    async fn list_tools(&mut self) -> Result<Vec<ToolDescriptor>> {
        let mut tools = vec![];
        let mut params = None;
        let mut cursors = std::collections::BTreeSet::new();
        loop {
            let value = self.request("tools/list", params).await?.into_result()?;
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
            .await?
            .into_result()
    }
}
