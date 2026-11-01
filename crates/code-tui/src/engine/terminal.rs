//! D144/D140 terminal adapter. Metadata ([`TermInfo`]) is ordinary frontend
//! data; terminal bytes travel only through [`TermMsg`] on their own channel,
//! never as a [`super::UiEvent`], and nothing here formats, logs or stores
//! them. User keystrokes go to the PTY through one ordered worker
//! ([`TermCmd`]); they are never recorded.
use super::Engine;
use code_engine::terminals::{ShareMode, TerminalEvent, TerminalInfo, TerminalKind};
use std::sync::mpsc as std_mpsc;
use tokio::sync::{broadcast, mpsc};

/// Terminal metadata (no input or output): identity, prompt, share mode.
#[derive(Clone, Debug)]
pub struct TermInfo {
    pub id: String,
    /// A private user terminal (D144); false is a D140 tty job, whose output
    /// the agent reads.
    pub user: bool,
    pub agent: String,
    pub breadcrumb: String,
    pub cwd: String,
    pub prompt: Option<String>,
    pub command: Option<String>,
    /// Output (not just status) is shared at close.
    pub share_output: bool,
    pub staged: usize,
    pub whenever: bool,
    /// running · exited · closed (user); running · done · failed · killed (job).
    pub status: String,
    pub exit: Option<i32>,
}

impl TermInfo {
    pub fn alive(&self) -> bool {
        self.status == "running"
    }
}

fn term_info(t: TerminalInfo) -> TermInfo {
    TermInfo {
        user: t.kind == TerminalKind::User,
        id: t.id,
        agent: t.agent_id,
        breadcrumb: t.breadcrumb,
        cwd: t.cwd.display().to_string(),
        prompt: t.prompt,
        command: t.command,
        share_output: t.share == ShareMode::Output,
        staged: t.shared_selection_bytes,
        whenever: t.wait.as_deref() == Some("whenever"),
        status: t.status,
        exit: t.exit_status,
    }
}

pub(super) fn translate(t: TerminalInfo) -> super::UiEvent {
    super::UiEvent::Terminal(term_info(t))
}

/// Terminal bytes for the sidebar's own screen only. Deliberately no
/// Debug/Clone/Serialize: it must never reach a log, the transcript or
/// any other UI state.
pub enum TermMsg {
    Output {
        id: String,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// The byte ring overflowed: views resynchronize (private: snapshot
    /// reset; jobs: explicit-offset paging).
    Lagged,
}

pub type TermRx = mpsc::Receiver<TermMsg>;

/// User actions on terminals, applied in order by one worker.
pub enum TermCmd {
    Input(String, String),
    Resize(String, u16, u16),
    Close(String, Option<String>),
}

pub type TermTx = mpsc::UnboundedSender<TermCmd>;

/// A private terminal's retained tail (≤256 KiB) for a view reset.
pub struct TermSnapshot {
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// One page of a job's output from an explicit offset.
pub struct JobPage {
    pub next: u64,
    pub total: u64,
    pub eof: bool,
    pub bytes: Vec<u8>,
}

impl Engine {
    /// Opt this session into private user terminals (D144). Called at open
    /// and after every switch, before the first turn.
    pub(super) fn enable_terminals(&self) -> Result<(), String> {
        self.session.enable_user_terminals().map_err(|e| e.message)
    }

    /// Subscribe to terminal bytes (before reading [`Engine::terminals`])
    /// and start the ordered input worker; errors go to `errors` (they
    /// never contain input).
    pub fn terminal_channels(&self, errors: std_mpsc::Sender<String>) -> (TermRx, TermTx) {
        let mut rx = self.session.subscribe_terminals();
        let (tx, out) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let msg = match rx.recv().await {
                    Ok(TerminalEvent::Output {
                        terminal_id,
                        offset,
                        bytes,
                    }) => TermMsg::Output {
                        id: terminal_id,
                        offset,
                        bytes,
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => TermMsg::Lagged,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        });
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<TermCmd>();
        let session = self.session.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                let result = match cmd {
                    TermCmd::Input(id, chars) => session.terminal_input(&id, &chars).await,
                    TermCmd::Resize(id, cols, rows) => {
                        session.terminal_resize(&id, cols, rows).await
                    }
                    TermCmd::Close(id, note) => {
                        session.terminal_close(&id, note).await.inspect_err(|e| {
                            let _ = errors.send(format!("terminal close: {}", e.message));
                        })
                    }
                };
                // Input/resize races with a closing shell are expected; they
                // carry no detail worth showing (and never the input).
                let _ = result;
            }
        });
        (out, cmd_tx)
    }

    pub fn terminals(&self) -> Vec<TermInfo> {
        self.session
            .terminals()
            .into_iter()
            .map(term_info)
            .collect()
    }

    pub fn terminal_snapshot(&self, id: &str) -> Result<TermSnapshot, String> {
        self.session
            .terminal_snapshot(id)
            .map(|s| TermSnapshot {
                offset: s.offset,
                bytes: s.bytes,
                truncated: s.truncated,
            })
            .map_err(|e| e.message)
    }

    pub fn terminal_job_page(&self, id: &str, offset: u64) -> Result<JobPage, String> {
        let json = self
            .session
            .terminal_job_output(id, offset, 64 * 1024)
            .map_err(|e| e.message)?;
        let v: serde_json::Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        Ok(JobPage {
            next: v["nextOffset"].as_u64().unwrap_or(offset),
            total: v["totalBytes"].as_u64().unwrap_or(0),
            eof: v["eof"].as_bool().unwrap_or(true),
            bytes: v["output"].as_str().unwrap_or_default().as_bytes().to_vec(),
        })
    }

    /// Output → status (more private); also revokes a staged excerpt.
    pub fn terminal_make_private(&self, id: &str) -> Result<(), String> {
        self.session
            .terminal_set_share(id, ShareMode::Status)
            .map_err(|e| e.message)
    }

    /// Stage (Some) or revoke (None) a hand-picked excerpt for sharing at close.
    pub fn terminal_stage(&self, id: &str, text: Option<String>) -> Result<(), String> {
        self.session
            .terminal_share_output(id, text)
            .map_err(|e| e.message)
    }
}
