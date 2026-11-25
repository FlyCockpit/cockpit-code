//! Owned foreground runners: conversion transfers ownership; execution never restarts.
use crate::{
    State,
    schema::*,
    tools::{CommitBarrier, ToolOutput},
};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct Sink {
    spool: File,
    job: Option<Arc<crate::jobs::JobHandle>>,
}
pub(crate) struct Output {
    sink: Mutex<Sink>,
    changed: tokio::sync::Notify,
    child: Mutex<Option<String>>,
    canonical: Mutex<Option<ToolCall>>,
}
impl Output {
    fn new(call: &ToolCall) -> std::io::Result<Self> {
        let output = Self {
            sink: Mutex::new(Sink {
                spool: tempfile::tempfile()?,
                job: None,
            }),
            changed: tokio::sync::Notify::new(),
            child: Mutex::default(),
            canonical: Mutex::default(),
        };
        *output.canonical.lock().unwrap() = Some(call.clone());
        Ok(output)
    }
    /// The dispatcher's resolved form of this run's call replaces the provisional one.
    pub fn revise(&self, call: &ToolCall) {
        let mut canonical = self.canonical.lock().unwrap();
        if canonical
            .as_ref()
            .is_some_and(|current| current.id == call.id)
        {
            *canonical = Some(call.clone());
        }
    }
    /// A foreground `task` run remembers the child agent it launched.
    pub fn launched_child(&self, id: &str) {
        if self
            .canonical
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|call| call.name == "task")
        {
            *self.child.lock().unwrap() = Some(id.into());
        }
    }
    /// Starts the job this run moves into with `start(call, command, child agent)`;
    /// returns it with the call as it now stands.
    fn start_job<J>(
        &self,
        call: &ToolCall,
        start: impl FnOnce(&ToolCall, String, Option<String>) -> Result<J, EngineError>,
    ) -> Result<(ToolCall, J), EngineError> {
        let child = self.child.lock().unwrap().clone();
        let canonical = self
            .canonical
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| call.clone());
        let command = canonical.arguments["command"]
            .as_str()
            .or(canonical.arguments["code"].as_str())
            .or(canonical.arguments["prompt"].as_str())
            .unwrap_or("")
            .into();
        let job = start(&canonical, command, child)?;
        Ok((canonical, job))
    }
    pub fn append(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut sink = self.sink.lock().unwrap();
        if let Some(job) = &sink.job {
            job.append(bytes)
        } else {
            sink.spool.write_all(bytes)
        }
    }
    fn adopt(&self, job: Arc<crate::jobs::JobHandle>) -> std::io::Result<()> {
        let mut sink = self.sink.lock().unwrap();
        sink.spool.seek(SeekFrom::Start(0))?;
        let mut buf = [0; 8192];
        loop {
            let n = sink.spool.read(&mut buf)?;
            if n == 0 {
                break;
            }
            job.append(&buf[..n])?;
        }
        sink.job = Some(job);
        self.changed.notify_waiters();
        Ok(())
    }
    pub fn is_background(&self) -> bool {
        self.sink.lock().unwrap().job.is_some()
    }
    pub async fn backgrounded(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.sink.lock().unwrap().job.is_some() {
                return;
            }
            changed.await;
        }
    }
}
fn failure(call: ToolCall, error: impl std::fmt::Display) -> ToolOutput {
    ToolOutput {
        details: Default::default(),
        canonical_call: call,
        output: format!("Error: {error}"),
        is_error: true,
    }
}
pub(crate) async fn execute(
    state: &State,
    call: ToolCall,
    cancel: CancellationToken,
    parent_barrier: Arc<CommitBarrier>,
) -> ToolOutput {
    let started = Instant::now();
    let output = match Output::new(&call) {
        Ok(output) => Arc::new(output),
        Err(error) => return failure(call, error),
    };
    let token = CancellationToken::new();
    let barrier = Arc::new(CommitBarrier::linked(parent_barrier));
    let mut owned = state.for_script(state.job_id.clone());
    owned.script_depth = state.script_depth;
    owned.tools = state.tools.for_foreground(output.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let control = state.control.clone();
    let owner = state.agent_id.clone();
    let relay = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            control.emit_for(&owner, event);
        }
    });
    let worker_call = call.clone();
    let worker_token = token.clone();
    let worker_barrier = barrier.clone();
    let mut worker = tokio::spawn(async move {
        crate::execute_raw(&owned, worker_call, worker_token, worker_barrier, &tx).await
    });
    loop {
        let changed = state.control.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let config = state.config.lock().unwrap().config().clone();
        let deadline = state
            .control
            .background_deadline(&state.agent_id, started, &config);
        let due = async {
            match deadline {
                Some((at, _)) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            result = &mut worker => { let _ = relay.await; return result.unwrap_or_else(|error| failure(call, error)); }
            _ = cancel.cancelled() => {
                barrier.revoke(); token.cancel();
                let result = worker.await;
                let _ = relay.await;
                return result.unwrap_or_else(|error| failure(call, error));
            }
            _ = &mut changed => {},
            _ = due => {
                // Recheck after timer wake: edit, cancellation and reclassification can withdraw it.
                let Some((at, reason)) = state.control.background_deadline(&state.agent_id, started, &config) else { continue; };
                if at > Instant::now() { continue; }
                let (mut canonical, job) = match output.start_job(&call, |canonical, command, child| state.control.start_job(&state.agent_id, &canonical.name, command, child, token.clone(), barrier.clone())) {
                    Ok(started) => started,
                    Err(error) => { barrier.revoke(); token.cancel(); let _ = worker.await; let _ = relay.await; return failure(call, error); }
                };
                state.control.guidance_job(&call.id, &job.id);
                if let Err(error) = output.adopt(job.clone()) {
                    barrier.revoke(); token.cancel(); let _ = worker.await; let _ = relay.await;
                    let _ = state.control.finish_job(&job.id, "failed", error.to_string(), None);
                    return failure(call, error);
                }
                if cancel.is_cancelled() || barrier.detach().is_err() {
                    barrier.revoke(); token.cancel(); let result = worker.await; let _ = relay.await;
                    let _ = state.control.finish_job(&job.id, "killed", "Cancelled before background adoption".into(), None);
                    return result.unwrap_or_else(|error| failure(call, error));
                }
                let _ = state.control.agent_status(&state.agent_id, "running");
                canonical.arguments["background"] = true.into();
                // The reducer updates both the logical call and provider-native replay before first replay.
                if let Err(error) = state.control.commit(&state.agent_id, HistoryEvent::CallUpdate {call: canonical.clone(), reason: format!("moved to background ({reason})")}) {
                    token.cancel(); barrier.revoke(); let _ = worker.await; let _ = relay.await;
                    let _ = state.control.finish_job(&job.id, "failed", error.to_string(), None);
                    return failure(call, error);
                }
                state.control.emit_for(&state.agent_id, EngineEvent::MovedToBackground {call_id: call.id, job_id: job.id.clone(), reason: reason.into()});
                let control = state.control.clone();
                let runner = job.clone();
                *job.task.lock().await = Some(tokio::spawn(async move {
                    let result = worker.await;
                    let _ = relay.await;
                    let (report, error) = match result {Ok(result) => (result.output, result.is_error), Err(error) => (error.to_string(), true)};
                    let task_status = serde_json::from_str::<serde_json::Value>(&report).ok().and_then(|v| v["status"].as_str().map(str::to_owned));
                    let status = if token.is_cancelled() || task_status.as_deref() == Some("killed") {"killed"} else if error || task_status.as_deref() == Some("failed") {"failed"} else {"done"};
                    let transcript = serde_json::from_str::<serde_json::Value>(&report).ok().and_then(|v| v["transcript"].as_str().map(str::to_owned));
                    let _ = runner.append(report.as_bytes());
                    let _ = control.finish_job(&runner.id, status, report, transcript);
                }));
                return ToolOutput { details: Default::default(),canonical_call: canonical, output: serde_json::json!({"jobId": job.id, "status":"running"}).to_string(), is_error: false};
            }
        }
    }
}
