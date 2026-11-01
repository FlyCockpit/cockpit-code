//! Bottom-up subtree wrap-up, without changing a lineage's frozen tools.
use crate::{Control, Session, schema::*, sessions::Journal};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

impl Control {
    pub(crate) fn is_finalizing(&self, agent: &str) -> bool {
        self.journal.lock().unwrap().finalizing.contains(agent)
    }

    pub(crate) fn finalize_subtree(
        &self,
        caller: &str,
        target: &str,
    ) -> Result<Value, EngineError> {
        if self.flat_mode() {
            return Err(EngineError::bad(
                "agents finalize is unavailable in flat mode",
            ));
        }
        let (target, ids, envelope) = {
            let mut j = self.journal.lock().unwrap();
            let target = j.resolve_agent_id(target)?;
            if !crate::agents::descendant(&j, caller, &target) {
                return Err(EngineError::bad(
                    "agents finalize is limited to your descendants",
                ));
            }
            let active = |j: &Journal, id: &str| {
                j.agents.get(id).is_some_and(|a| {
                    !a.hidden()
                        && (matches!(
                            a.status.as_str(),
                            "running" | "waiting" | "compacting" | "roadblocked"
                        ) || a.status.starts_with("queued"))
                })
            };
            if !active(&j, &target) {
                return Err(EngineError::bad(
                    "target is already terminal; inspect it with agents show",
                ));
            }
            let mut ids = j
                .agents
                .keys()
                .filter(|id| {
                    (**id == target || crate::agents::descendant(&j, &target, id)) && active(&j, id)
                })
                .cloned()
                .collect::<Vec<_>>();
            ids.sort_by_key(|id| {
                std::cmp::Reverse((
                    crate::subtree::depth(&j, id),
                    j.agents[id].created_at.clone(),
                ))
            });
            let envelope = j.append(
                caller,
                HistoryEvent::AgentFinalizeRequested {
                    target: target.clone(),
                    agents: ids.clone(),
                },
            )?;
            (target, ids, envelope)
        };
        self.emit(EngineEvent::Committed {
            envelope: envelope.clone(),
        });
        if let Some(record) = crate::decisions::automatic(&envelope) {
            self.record_decision(record)?;
        }
        self.changed.notify_waiters();
        Ok(json!({"agentId":target,"status":"finalizing","agents":ids}))
    }

    pub(crate) async fn finalize_boundary(
        &self,
        agent: &str,
        cancel: &CancellationToken,
    ) -> Result<(), EngineError> {
        self.scheduler.park(agent);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let pending = self.runtime.children_pending(agent);
            if !pending {
                break;
            }
            tokio::select! {
                _ = &mut changed => {},
                _ = cancel.cancelled() => return Err(EngineError::cancelled()),
            }
        }
        self.scheduler.wake(agent);
        self.commit(agent, HistoryEvent::AgentFinalTurn)?;
        self.commit(agent, HistoryEvent::SystemReminder { text: format!(
            "Your subtree is finalizing. All children have reported. This is your one final turn, with tools disabled. Write your final report to your parent now: completed work, child results, evidence, remaining work and blockers. Do not call tools.\n{}",
            crate::progress::facts(self, agent),
        ) })?;
        Ok(())
    }
}

impl Session {
    /// Request graceful bottom-up finalization of a descendant; completion arrives
    /// through ordinary child reports/jobs. Does not cancel an in-flight call.
    pub fn finalize_agent(&self, id: &str) -> Result<Value, EngineError> {
        self.control.finalize_subtree("root", id)
    }
}
