//! Frontend terminal transport. Byte events deliberately cannot be serialized.
use crate::{Session, schema::EngineError};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, sync::Mutex};
use tokio::sync::broadcast;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInfo {
    pub id: String,
    pub agent_id: String,
    pub job_id: Option<String>,
    pub cwd: PathBuf,
    pub status: String,
    pub kind: TerminalKind,
    pub breadcrumb: String,
    pub prompt: Option<String>,
    pub command: Option<String>,
    pub share: ShareMode,
    pub wait: Option<String>,
    pub exit_status: Option<i32>,
    pub shared_selection_bytes: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalKind {
    Job,
    User,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareMode {
    #[default]
    Status,
    Output,
}
/// A snapshot is UI-only, like TerminalEvent. Never put it in a transcript.
pub struct TerminalSnapshot {
    pub terminal: TerminalInfo,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}
impl std::fmt::Debug for TerminalSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TerminalSnapshot(<redacted>)")
    }
}
/// UI-only bytes, never route through EngineEvent, journals or transcript/OSC.
#[derive(Clone)]
pub enum TerminalEvent {
    Output {
        terminal_id: String,
        offset: u64,
        bytes: Vec<u8>,
    },
}
impl std::fmt::Debug for TerminalEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TerminalEvent::Output(<redacted>)")
    }
}
pub(crate) struct Hub {
    pub infos: Mutex<BTreeMap<String, TerminalInfo>>,
    pub bytes: broadcast::Sender<TerminalEvent>,
    pub user: Mutex<BTreeMap<String, std::sync::Arc<crate::user_terminal::UserTerminal>>>,
    pub enabled: std::sync::atomic::AtomicBool,
    pub work: Mutex<crate::TitleWork>,
}
impl Default for Hub {
    fn default() -> Self {
        Self {
            infos: Mutex::default(),
            bytes: broadcast::channel(256).0,
            user: Mutex::default(),
            enabled: std::sync::atomic::AtomicBool::new(false),
            work: Mutex::default(),
        }
    }
}
impl Session {
    pub fn subscribe_terminals(&self) -> broadcast::Receiver<TerminalEvent> {
        self.control.terminals.bytes.subscribe()
    }
    pub fn terminals(&self) -> Vec<TerminalInfo> {
        self.control
            .terminals
            .infos
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }
    pub async fn terminal_input(&self, id: &str, chars: &str) -> Result<(), EngineError> {
        if let Some(terminal) = self.control.user_terminal(id) {
            self.control.check_owner()?;
            return terminal.input(chars).await;
        }
        let id = self.resolve_job_id(id)?;
        self.control.write_job_input(None, &id, chars).await
    }
    pub async fn terminal_resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), EngineError> {
        self.control.check_owner()?;
        if let Some(terminal) = self.control.user_terminal(id) {
            return terminal.resize(cols, rows);
        }
        let id = self.resolve_job_id(id)?;
        let handle = self
            .control
            .runtime
            .jobs
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| EngineError::bad("job is not live"))?;
        let input = handle.input.lock().await;
        match input.as_ref() {
            Some(crate::jobs::Input::Pty(pty)) => pty
                .resize(cols, rows)
                .map_err(|e| EngineError::bad(e.to_string())),
            _ => Err(EngineError::bad("job has no PTY")),
        }
    }
    pub fn terminal_job_output(
        &self,
        id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<String, EngineError> {
        self.control
            .jobs_output(&self.resolve_job_id(id)?, offset, limit)
    }
}
