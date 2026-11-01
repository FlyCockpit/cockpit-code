//! Session-owned background jobs. Output is streamed to disk; status is canonical history.
use crate::{Control, schema::*, sessions, tools::CommitBarrier};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

pub(crate) struct JobHandle {
    pub id: String,
    pub cancel: CancellationToken,
    pub barrier: Arc<CommitBarrier>,
    output: Mutex<File>,
    pub(crate) completed: AtomicBool,
    done: tokio::sync::Notify,
    pub(crate) task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(crate) input: tokio::sync::Mutex<Option<Input>>,
    input_ready: tokio::sync::Notify,
    read_cursor: Mutex<u64>,
    pub(crate) terminal_tx:
        Mutex<Option<tokio::sync::broadcast::Sender<crate::terminals::TerminalEvent>>>,
}
pub(crate) enum Input {
    Pty(Arc<crate::pty::Pty>),
    Pipe(tokio::process::ChildStdin),
}
impl JobHandle {
    pub async fn set_input(&self, input: Input) {
        *self.input.lock().await = Some(input);
        self.input_ready.notify_waiters();
    }
    pub fn append(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut output = self.output.lock().unwrap();
        let offset = output.metadata()?.len();
        output.write_all(bytes)?;
        if let Some(tx) = &*self.terminal_tx.lock().unwrap() {
            let _ = tx.send(crate::terminals::TerminalEvent::Output {
                terminal_id: self.id.clone(),
                offset,
                bytes: bytes.to_vec(),
            });
        }
        Ok(())
    }
}
impl Control {
    pub(crate) fn start_job(
        &self,
        owner: &str,
        kind: &str,
        command: String,
        child: Option<String>,
        cancel: CancellationToken,
        barrier: Arc<CommitBarrier>,
    ) -> Result<Arc<JobHandle>, EngineError> {
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.journal.lock().unwrap().dir.join("jobs");
        std::fs::create_dir_all(&dir).map_err(|e| EngineError::bad(e.to_string()))?;
        let output_path = if kind == "bash" {
            let outputs = self.outputs_dir();
            std::fs::create_dir_all(&outputs).map_err(|e| EngineError::bad(e.to_string()))?;
            Some(outputs.join(format!("{id}.txt")))
        } else {
            None
        };
        let output = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(
                output_path
                    .clone()
                    .unwrap_or_else(|| dir.join(format!("{id}.log"))),
            )
            .map_err(|e| EngineError::bad(e.to_string()))?;
        let now = sessions::now();
        let job = JobRecord {
            id: id.clone(),
            owner: owner.into(),
            kind: kind.into(),
            status: "running".into(),
            command,
            agent_id: child,
            started: now.clone(),
            updated: now,
            output_bytes: 0,
            output_path,
            agent: None,
            report: None,
            transcript: None,
        };
        self.commit(owner, HistoryEvent::JobUpdate { job: job.clone() })?;
        let handle = Arc::new(JobHandle {
            id: id.clone(),
            cancel,
            barrier,
            output: Mutex::new(output),
            completed: AtomicBool::new(false),
            done: tokio::sync::Notify::new(),
            task: tokio::sync::Mutex::new(None),
            input: tokio::sync::Mutex::new(None),
            input_ready: tokio::sync::Notify::new(),
            read_cursor: Mutex::new(0),
            terminal_tx: Mutex::new(None),
        });
        self.runtime.jobs.lock().unwrap().insert(id, handle.clone());
        self.emit(EngineEvent::Job { job });
        Ok(handle)
    }
    /// Completed foreground output: no running handle, completion mail or extra
    /// process. Persist raw bytes before exposing the paging handle.
    pub(crate) fn store_bash_output(
        &self,
        owner: &str,
        command: &str,
        call_id: &str,
        spool: &mut File,
        success: bool,
    ) -> Result<String, EngineError> {
        let id = uuid::Uuid::new_v4().to_string();
        let path = self.save_output(call_id, spool)?;
        let bytes = std::fs::metadata(&path)
            .map_err(|e| EngineError::bad(e.to_string()))?
            .len();
        let now = sessions::now();
        let job = JobRecord {
            id: id.clone(),
            owner: owner.into(),
            kind: "bash-output".into(),
            status: if success { "done" } else { "failed" }.into(),
            command: command.into(),
            agent_id: None,
            started: now.clone(),
            updated: now,
            output_bytes: bytes,
            output_path: Some(path.clone()),
            agent: None,
            report: None,
            transcript: None,
        };
        if let Err(error) = self.commit(owner, HistoryEvent::JobUpdate { job }) {
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok(id)
    }
    pub(crate) fn job_record(&self, id: &str) -> Result<JobRecord, EngineError> {
        let j = self.journal.lock().unwrap();
        let mut row = j
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::bad("job not found"))?;
        row.output_bytes = std::fs::metadata(
            row.output_path
                .clone()
                .unwrap_or_else(|| j.dir.join("jobs").join(format!("{id}.log"))),
        )
        .map(|m| m.len())
        .unwrap_or(row.output_bytes);
        if let Some(id) = &row.agent_id
            && j.agents.contains_key(id)
        {
            let contexts = j.dir.join("agents").join(id).join("contexts");
            row.transcript = Some(contexts.display().to_string());
            row.output_bytes = std::fs::read_dir(&contexts)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum();
            row.agent = Some(crate::agents::row(&j, &row.owner, id));
        }
        Ok(row)
    }
    pub(crate) fn finish_job(
        &self,
        id: &str,
        status: &str,
        mut report: String,
        transcript: Option<String>,
    ) -> Result<(), EngineError> {
        report = crate::tools::cap(&report);
        let flag = self.check_reviewer_guidance(None, Some(id), true);
        if !flag.is_empty() {
            report.push_str(&flag);
            if let Some(handle) = self.runtime.jobs.lock().unwrap().get(id) {
                let _ = handle.append(flag.as_bytes());
            }
        }
        let result = (|| {
            if let Some(handle) = self.runtime.jobs.lock().unwrap().get(id) {
                handle
                    .output
                    .lock()
                    .unwrap()
                    .sync_all()
                    .map_err(|e| EngineError::bad(e.to_string()))?;
            }
            let mut job = self.job_record(id)?;
            if !job.running() {
                return Ok(());
            }
            job.agent = None;
            job.status = status.into();
            job.updated = sessions::now();
            job.report = Some(report);
            job.transcript = transcript;
            self.commit(&job.owner, HistoryEvent::JobUpdate { job: job.clone() })?;
            self.message(
                "jobs",
                &job.owner,
                format!(
                    "Job {} {} ({}): {}",
                    job.id,
                    job.status,
                    job.kind,
                    job.report.as_deref().unwrap_or("")
                ),
            )?;
            self.emit(EngineEvent::Job { job });
            Ok(())
        })();
        if let Some(handle) = self.runtime.jobs.lock().unwrap().get(id) {
            handle.completed.store(true, Ordering::Release);
            handle.done.notify_waiters();
            handle.input_ready.notify_waiters();
        }
        let terminal = {
            let mut infos = self.terminals.infos.lock().unwrap();
            infos.get_mut(id).map(|info| {
                info.status = status.into();
                info.clone()
            })
        };
        if let Some(terminal) = terminal {
            self.emit(EngineEvent::TerminalChanged { terminal });
        }
        self.changed.notify_waiters();
        result
    }
    pub(crate) fn running_owned(&self, owner: &str) -> Vec<JobRecord> {
        self.journal
            .lock()
            .unwrap()
            .jobs
            .values()
            .filter(|j| j.owner == owner && j.running())
            .cloned()
            .collect()
    }
    pub(crate) fn jobs_output(
        &self,
        id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<String, EngineError> {
        let job = self.job_record(id)?;
        let path = job.output_path.clone().unwrap_or_else(|| {
            self.journal
                .lock()
                .unwrap()
                .dir
                .join("jobs")
                .join(format!("{id}.log"))
        });
        let mut file = File::open(&path).map_err(|e| EngineError::bad(e.to_string()))?;
        let total_bytes = file
            .metadata()
            .map_err(|e| EngineError::bad(e.to_string()))?
            .len();
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| EngineError::bad(e.to_string()))?;
        let mut bytes = Vec::new();
        file.take(limit.clamp(1, 1024 * 1024))
            .read_to_end(&mut bytes)
            .map_err(|e| EngineError::bad(e.to_string()))?;
        // Budget the serialized result, including JSON escaping. Never claim
        // bytes hidden by the outer tool cap or shell compression were returned.
        let mut count = bytes.len();
        loop {
            if let Err(error) = std::str::from_utf8(&bytes[..count])
                && error.error_len().is_none()
                && error.valid_up_to() > 0
            {
                count = error.valid_up_to();
            }
            let next = offset.saturating_add(count as u64);
            let result = serde_json::json!({"jobId":id,"status":job.status,"offset":offset,"nextOffset":next,"totalBytes":total_bytes,"eof":next >= total_bytes,"output":String::from_utf8_lossy(&bytes[..count]),"fullOutputPath":if total_bytes > 8192 {Some(&path)} else {None}}).to_string();
            if result.len() <= 7500 || count <= 1 {
                return Ok(result);
            }
            count = count.div_ceil(2);
        }
    }
    pub(crate) async fn kill_job(&self, owner: &str, id: &str) -> Result<(), EngineError> {
        let record = self.job_record(id)?;
        if record.owner != owner {
            return Err(EngineError::bad("only a job's owner may kill it"));
        }
        let handle = self.runtime.jobs.lock().unwrap().get(id).cloned();
        if let Some(job) = handle {
            if record.running() {
                job.barrier.revoke();
                job.cancel.cancel();
            }
            // Wait until process groups, child agents and file workers have all joined.
            loop {
                let notified = job.done.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if job.completed.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
            if let Some(task) = job.task.lock().await.take() {
                task.await
                    .map_err(|e| EngineError::bad(format!("job runner failed: {e}")))?;
            }
        }
        // A historical task job may refer to an agent now running a later follow-up.
        let running = record.running();
        if let Some(id) = record.agent_id.filter(|_| running) {
            let child = self.runtime.agents.lock().unwrap().get(&id).cloned();
            if let Some(child) = child {
                child.join().await?;
            }
        }
        Ok(())
    }
    pub(crate) async fn write_job_input(
        &self,
        owner: Option<&str>,
        id: &str,
        chars: &str,
    ) -> Result<(), EngineError> {
        use tokio::io::AsyncWriteExt;
        self.check_owner()?;
        let record = self.job_record(id)?;
        if owner.is_some_and(|owner| owner != record.owner) {
            return Err(EngineError::bad("only a job's owner may send input"));
        }
        if chars.is_empty() {
            return Ok(());
        }
        if record.kind != "bash" {
            return Err(EngineError::bad("only bash jobs accept input"));
        }
        if chars.len() > 64 * 1024 {
            return Err(EngineError::bad("input exceeds 64 KiB"));
        }
        let job = self
            .runtime
            .jobs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::bad("job is not live"))?;
        loop {
            let ready = job.input_ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            let mut input = job.input.lock().await;
            if job.completed.load(Ordering::Acquire) || !self.job_record(id)?.running() {
                return Err(EngineError::bad("job has finished"));
            }
            if let Some(input) = input.as_mut() {
                return tokio::select! {
                    _ = job.cancel.cancelled() => Err(EngineError::cancelled()),
                    result = async { match input {
                        Input::Pty(pty) => pty.write(chars.as_bytes()).await,
                        Input::Pipe(pipe) => pipe.write_all(chars.as_bytes()).await,
                    } } => result.map_err(|e| EngineError::bad(e.to_string())),
                };
            }
            drop(input);
            ready.await;
        }
    }
    pub(crate) fn read_job_input(&self, id: &str) -> Result<String, EngineError> {
        let handle = self
            .runtime
            .jobs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| {
                EngineError::bad("job is not live; use jobs.output for historical output")
            })?;
        let mut cursor = handle.read_cursor.lock().unwrap();
        let result = self.jobs_output(id, *cursor, 8192)?;
        let page: serde_json::Value =
            serde_json::from_str(&result).map_err(|e| EngineError::bad(e.to_string()))?;
        *cursor = page["nextOffset"].as_u64().unwrap_or(*cursor);
        Ok(result)
    }
    pub(crate) async fn wait_job_yield(
        &self,
        id: &str,
        milliseconds: u64,
        cancel: &CancellationToken,
    ) -> Result<String, EngineError> {
        let handle = self
            .runtime
            .jobs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::bad("job is not live"))?;
        let done = handle.done.notified();
        tokio::pin!(done);
        done.as_mut().enable();
        if !handle.completed.load(Ordering::Acquire) {
            tokio::select! {
                _ = &mut done => {},
                _ = tokio::time::sleep(std::time::Duration::from_millis(milliseconds)) => {},
                _ = cancel.cancelled() => { self.kill_job(&self.job_record(id)?.owner, id).await?; return Err(EngineError::cancelled()); },
            }
        }
        self.read_job_input(id)
    }
}
