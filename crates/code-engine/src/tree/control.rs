//! Agent-tree operations on `Control`: stopping, messaging, questions and owned work.
use super::*;

impl Control {
    pub(crate) async fn stop_subtree(
        &self,
        caller: &str,
        target: &str,
        reason: &str,
    ) -> Result<Value, EngineError> {
        if reason.trim().is_empty() {
            return Err(EngineError::bad("stop reason must not be empty"));
        }
        let (target, parent, ids) = {
            let j = self.journal.lock().unwrap();
            let target = j.resolve_agent_id(target)?;
            if !crate::agents::descendant(&j, caller, &target) {
                return Err(EngineError::bad(
                    "agents stop is limited to your descendants",
                ));
            }
            if !matches!(
                j.agents[&target].status.as_str(),
                "running" | "waiting" | "compacting" | "roadblocked"
            ) && !j.agents[&target].status.starts_with("queued")
            {
                return Err(EngineError::bad(
                    "target is already terminal; inspect it with agents show",
                ));
            }
            let parent = j.agents[&target].parent_id.clone().unwrap();
            let ids = j
                .agents
                .keys()
                .filter(|id| *id == &target || crate::agents::descendant(&j, &target, id))
                .cloned()
                .collect::<Vec<_>>();
            (target, parent, ids)
        };
        let runs = {
            let runtime = self.runtime.agents.lock().unwrap();
            ids.iter()
                .filter_map(|id| runtime.get(id).cloned())
                .collect::<Vec<_>>()
        };
        for run in &runs {
            run.barrier.revoke();
            run.cancel.cancel();
        }
        for run in runs {
            run.join().await?;
        }
        let report = format!(
            "Stopped by {caller}: {reason}\nPartial progress:\n{}",
            crate::progress::facts(self, &target)
        );
        self.commit(
            caller,
            HistoryEvent::AgentStopped {
                target: target.clone(),
                reason: reason.into(),
                report: report.clone(),
            },
        )?;
        self.message(&target, &parent, report.clone())?;
        Ok(
            json!({"id":crate::ids::short(&target),"agentId":target,"state":"stopped","reason":reason,"stopped":ids,"partialProgress":report}),
        )
    }
    pub(crate) fn breadcrumb(&self, id: &str) -> String {
        session_tools::breadcrumb(&self.journal.lock().unwrap().agents, id)
    }
    pub(crate) fn emit_for(&self, id: &str, event: EngineEvent) {
        if id == "root"
            || matches!(
                event,
                EngineEvent::Usage { .. } | EngineEvent::Committed { .. }
            )
        {
            self.emit(event);
        } else {
            self.emit(EngineEvent::AgentEvent {
                agent_id: id.into(),
                breadcrumb: self.breadcrumb(id),
                event: Box::new(event),
            });
        }
    }
    pub(crate) fn agent_status(&self, id: &str, status: &str) -> Result<(), EngineError> {
        self.agent_status_question(id, status, None)
    }
    fn agent_status_question(
        &self,
        id: &str,
        status: &str,
        question_id: Option<&str>,
    ) -> Result<(), EngineError> {
        let agent = {
            let mut j = self.journal.lock().unwrap();
            let mut agent = j
                .agents
                .get(id)
                .cloned()
                .ok_or_else(|| EngineError::bad("agent not found"))?;
            if agent.status == status && question_id.is_none() {
                return Ok(());
            }
            agent.status = status.into();
            j.update_agent(agent.clone())?;
            agent
        };
        self.commit(id, HistoryEvent::AgentState { agent })?;
        self.emit(EngineEvent::AgentStatus {
            agent_id: id.into(),
            status: status.into(),
            breadcrumb: self.breadcrumb(id),
            question_id: question_id.map(str::to_owned),
        });
        self.scope_owner_ended(id, status)?;
        self.changed.notify_waiters();
        Ok(())
    }
    pub(crate) async fn wait_question(
        &self,
        asker: &str,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<String, EngineError> {
        self.scheduler.park(asker);
        self.agent_status_question(asker, "waiting", Some(id))?;
        self.goal_clock
            .lock()
            .unwrap()
            .wait(format!("question:{id}"));
        let mut result = async {
            let mut owner_check = tokio::time::interval(Duration::from_millis(250));
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                self.check_owner()?;
                if cancel.is_cancelled() {
                    return Err(EngineError::cancelled());
                }
                {
                    let j = self.journal.lock().unwrap();
                    if let Some(answer) = j.questions.get(id).and_then(|q| q.answer.clone()) {
                        return Ok(answer);
                    }
                    if j.agents
                        .get(asker)
                        .is_none_or(|a| matches!(a.status.as_str(), "done" | "failed" | "killed"))
                    {
                        return Err(EngineError::cancelled());
                    }
                }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(EngineError::cancelled()),
                    _ = &mut notified => {},
                    _ = owner_check.tick() => {},
                }
            }
        }
        .await;
        self.goal_clock
            .lock()
            .unwrap()
            .resume(&format!("question:{id}"));
        // Atomically downgrade an unanswered park before a later answer can
        // arrive. If an answer won the cancellation race, retain it in the tool
        // result; otherwise future answers use whenever delivery.
        if result.is_err() {
            let released = self.release_question_wait(id);
            if result
                .as_ref()
                .is_err_and(|e| e.kind == ErrorKind::Unavailable && e.message == "turn cancelled")
                && let Ok(Some(answer)) = released
            {
                result = Ok(answer);
            }
        }
        self.scheduler.wake(asker);
        // Owner loss may prevent persistence. Preserve the original failure.
        let waiting = self
            .journal
            .lock()
            .unwrap()
            .agents
            .get(asker)
            .is_some_and(|a| a.status == "waiting");
        if waiting {
            let restored = self.agent_status(asker, "running");
            if result.is_ok() {
                restored?;
            }
        }
        result
    }
    fn release_question_wait(&self, id: &str) -> Result<Option<String>, EngineError> {
        let update = {
            let mut j = self.journal.lock().unwrap();
            let Some(mut q) = j.questions.get(id).cloned() else {
                return Ok(None);
            };
            if let Some(answer) = &q.answer {
                return Ok(Some(answer.clone()));
            }
            if !q.blocking {
                return Ok(None);
            }
            q.blocking = false;
            let envelope = j.append(
                &q.asker,
                HistoryEvent::Question {
                    question: q.clone(),
                },
            )?;
            (q, envelope)
        };
        self.emit(EngineEvent::Committed { envelope: update.1 });
        self.emit(EngineEvent::Question { question: update.0 });
        self.changed.notify_waiters();
        Ok(None)
    }
    pub(crate) fn message(&self, from: &str, to: &str, text: String) -> Result<(), EngineError> {
        {
            let j = self.journal.lock().unwrap();
            if !j.agents.contains_key(to) || (from != "jobs" && !j.agents.contains_key(from)) {
                return Err(EngineError::bad("message agent not found"));
            }
        }
        let text = if from == "user"
            || self
                .journal
                .lock()
                .unwrap()
                .agents
                .get(to)
                .and_then(|a| a.parent_id.as_deref())
                == Some(from)
        {
            format!("{text}{}", self.context_tag(to, &text))
        } else {
            text
        };
        let event = HistoryEvent::Message {
            from: from.into(),
            to: to.into(),
            text: text.clone(),
        };
        if from != to && from != "jobs" {
            self.commit(from, event.clone())?;
        }
        {
            let mut mail_since = self.mail_since.lock().unwrap();
            self.commit(to, event)?;
            if from != "jobs" {
                mail_since
                    .entry(to.into())
                    .or_insert_with(tokio::time::Instant::now);
            }
        }
        self.emit(EngineEvent::AgentMessage {
            from: from.into(),
            to: to.into(),
            text,
            breadcrumb: self.breadcrumb(to),
        });
        self.changed.notify_waiters();
        Ok(())
    }
    pub(crate) fn has_mail(&self, id: &str) -> bool {
        self.journal
            .lock()
            .unwrap()
            .mailbox
            .get(id)
            .is_some_and(|q| !q.is_empty())
    }
    pub(crate) fn answer_question(&self, id: &str, text: String) -> Result<(), EngineError> {
        let q = self
            .journal
            .lock()
            .unwrap()
            .questions
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::bad("question not found"))?;
        self.answer_set(id, crate::questions::text_answers(&q, &text))
    }
    pub(crate) fn escalate_question(
        &self,
        id: &str,
        note: Option<String>,
    ) -> Result<(), EngineError> {
        let result = {
            let mut j = self.journal.lock().unwrap();
            (|| {
                let mut q = j
                    .questions
                    .get(id)
                    .ok_or_else(|| EngineError::bad("question not found"))?
                    .clone();
                if q.answer.is_some() {
                    return Err(EngineError::bad("question already answered"));
                }
                if q.recipient.is_none() {
                    return Err(EngineError::bad(
                        "question is already addressed to the user",
                    ));
                }
                q.recipient = q
                    .recipient
                    .as_ref()
                    .and_then(|id| j.agents.get(id))
                    .and_then(|a| a.parent_id.clone());
                q.note = note;
                let envelope = j.append(
                    &q.asker,
                    HistoryEvent::Question {
                        question: q.clone(),
                    },
                )?;
                Ok((q, envelope))
            })()
        };
        let (q, envelope) = result.inspect_err(|e| self.claimed(e))?;
        self.emit(EngineEvent::Committed { envelope });
        if let Some(parent) = &q.recipient {
            self.message(
                &q.asker,
                parent,
                format!(
                    "<question id=\"{}\" from=\"{}\">{} (escalation note: {})</question>",
                    q.id,
                    q.asker,
                    serde_json::to_string(&q.questions).unwrap(),
                    q.note.as_deref().unwrap_or("")
                ),
            )?;
        }
        self.emit(EngineEvent::Question { question: q });
        self.changed.notify_waiters();
        Ok(())
    }
    pub(crate) fn background_snapshot(&self, owner: &str) -> String {
        let j = self.journal.lock().unwrap();
        let children=j.agents.values().filter(|a|a.parent_id.as_deref()==Some(owner)&&(matches!(a.status.as_str(),"running"|"waiting"|"compacting") || a.status.starts_with("queued"))).map(|a|json!({"id":a.agent_id,"agent":a.agent,"model":a.model,"status":a.status,"task":j.executions.get(&a.agent_id).map(|e|&e.task),"lastMessage":j.histories.get(&a.agent_id).and_then(|h|h.iter().rev().find_map(|e|if let HistoryEvent::Message{text,..}=e{Some(text)}else{None}))})).collect::<Vec<_>>();
        let jobs = j
            .jobs
            .values()
            .filter(|job| job.owner == owner && job.running())
            .map(|job| {
                let mut value = json!(job);
                if let Ok(metadata) =
                    std::fs::metadata(j.dir.join("jobs").join(format!("{}.log", job.id)))
                {
                    value["outputBytes"] = json!(metadata.len());
                }
                value
            })
            .collect::<Vec<_>>();
        format!(
            "Running background work: {}",
            json!({"subagents":children,"jobs":jobs})
        )
    }
    pub(crate) async fn finish_owned(&self, owner: &str) {
        let jobs = self
            .journal
            .lock()
            .unwrap()
            .jobs
            .values()
            .filter(|j| j.owner == owner)
            .cloned()
            .collect::<Vec<_>>();
        let mut joined = jobs
            .iter()
            .map(|j| j.id.clone())
            .collect::<std::collections::HashSet<_>>();
        for job in jobs {
            let _ = self.kill_job(owner, &job.id).await;
        }
        // A cancelled script may have launched another owned job just before cancellation.
        // Re-snapshot after joining the original workers so that job is joined as well.
        loop {
            let remaining = self
                .running_owned(owner)
                .into_iter()
                .filter(|j| joined.insert(j.id.clone()))
                .collect::<Vec<_>>();
            if remaining.is_empty() {
                break;
            }
            for job in remaining {
                let _ = self.kill_job(owner, &job.id).await;
            }
        }
        let children = self
            .runtime
            .agents
            .lock()
            .unwrap()
            .values()
            .filter(|a| a.parent == owner)
            .cloned()
            .collect::<Vec<_>>();
        for child in children {
            if child.result.lock().unwrap().is_none() {
                child.barrier.revoke();
                child.cancel.cancel();
                let _ = child.wait(&CancellationToken::new()).await;
            }
            let _ = child.join().await;
        }
        self.script_values.lock().unwrap().remove(owner);
    }
    pub(crate) fn restore_interrupted(&self) -> Result<(), EngineError> {
        let (jobs, agents) = {
            let j = self.journal.lock().unwrap();
            (
                j.jobs
                    .values()
                    .filter(|j| j.running())
                    .cloned()
                    .collect::<Vec<_>>(),
                j.agents
                    .values()
                    .filter(|a| {
                        a.agent_id != "root"
                            && (matches!(a.status.as_str(), "running" | "waiting" | "compacting")
                                || a.status.starts_with("queued"))
                    })
                    .map(|a| a.agent_id.clone())
                    .collect::<Vec<_>>(),
            )
        };
        for mut job in jobs {
            job.status = "interrupted".into();
            job.updated = sessions::now();
            job.report=Some("Previous owner process stopped; background work was interrupted and will not be relaunched.".into());
            self.commit(&job.owner.clone(), HistoryEvent::JobUpdate { job })?;
        }
        for id in agents {
            self.agent_status(&id, "failed")?;
        }
        // No parked future survives process ownership. Retain unanswered
        // questions, but allow any later answer to reach a resumed agent.
        let questions = self
            .journal
            .lock()
            .unwrap()
            .questions
            .values()
            .filter(|q| q.blocking && q.answer.is_none())
            .map(|q| q.id.clone())
            .collect::<Vec<_>>();
        for id in questions {
            self.release_question_wait(&id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_answer_uses_the_structured_answer_journal() {
        let (temp, store) = crate::test_support::store(serde_json::json!({}));
        let session = Session::create(store, temp.path().to_owned(), Default::default()).unwrap();
        let id = session
            .control
            .ask_question("root", "Proceed?".into(), false)
            .unwrap();
        session.control.answer_question(&id, "yes".into()).unwrap();
        assert!(
            session
                .control
                .answer_question(&id, "again".into())
                .is_err()
        );
        assert!(
            session
                .control
                .answer_question("unknown", "yes".into())
                .is_err()
        );
        assert_eq!(
            session.control.journal.lock().unwrap().questions[&id]
                .answers
                .as_ref()
                .unwrap()[0]
                .text
                .as_deref(),
            Some("yes")
        );
        session.shutdown().await.unwrap();
    }
}
