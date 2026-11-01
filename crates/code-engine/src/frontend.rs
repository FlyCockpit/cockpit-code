//! Front-end-only session operations (TUI lane tui-v3): user steering of a
//! running subagent and user kill of any background job.
use crate::{Session, schema::*};

impl Session {
    /// Steer a running subagent from the user. The text is queued in the
    /// child's mailbox like a parent `message` and reaches it as a reminder at
    /// its next turn boundary. The sender is recorded as `user`.
    pub fn steer(&self, agent_id: &str, text: impl Into<String>) -> Result<(), EngineError> {
        let agent_id = self.resolve_agent_id(agent_id)?;
        let agent_id = agent_id.as_str();
        let text = text.into();
        {
            let finished = self
                .control
                .journal
                .lock()
                .unwrap()
                .agents
                .get(agent_id)
                .is_some_and(|a| matches!(a.status.as_str(), "done" | "failed"));
            if finished {
                let mut work = self.control.title_work.lock().unwrap();
                if work.closing {
                    return Err(EngineError::bad("session is shutting down"));
                }
                let cancel = work.cancel.child_token();
                let id = agent_id.to_owned();
                let state = self.state.clone();
                let control = self.control.clone();
                self.resume_discussion(agent_id);
                work.tasks.retain(|task| !task.is_finished());
                work.tasks.push(tokio::spawn(async move {
                    let result = async {
                        let state = tokio::select! { _ = cancel.cancelled() => return Err(EngineError::cancelled()), state = state.lock() => state };
                        crate::tree::resume_discussion_child(&state, &id, text, &cancel).await
                    }.await;
                    if let Err(error) = result { control.emit(EngineEvent::Error { error }); }
                }));
                return Ok(());
            }
        }
        let alive = {
            let j = self.control.journal.lock().unwrap();
            let agent = j
                .agents
                .get(agent_id)
                .ok_or_else(|| EngineError::bad("agent not found"))?;
            agent.parent_id.is_some()
                && (matches!(agent.status.as_str(), "running" | "waiting" | "compacting")
                    || agent.status.starts_with("queued"))
        };
        if !alive {
            return Err(EngineError::bad(
                "only a running subagent can be steered; send to root instead",
            ));
        }
        self.queue_message(agent_id, text, code_config::QueueClass::Steer)
            .map(|_| ())
    }

    /// Kill a running background job on the user's behalf, whoever owns it,
    /// and wait until it has fully stopped.
    pub async fn kill_job(&self, id: &str) -> Result<(), EngineError> {
        let id = self.resolve_job_id(id)?;
        let id = id.as_str();
        let owner = self.control.job_record(id)?.owner;
        self.control.kill_job(&owner, id).await
    }
}
