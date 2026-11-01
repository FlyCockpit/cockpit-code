//! Root mailbox delivery, independent of the frontend's turn receiver.
use crate::{Action, Control, Session, schema::EngineError};
use std::sync::Arc;
use tokio::time::{Duration, sleep};

impl Control {
    pub(crate) fn root_mail_pending(&self) -> bool {
        self.journal
            .lock()
            .unwrap()
            .mailbox
            .get("root")
            .is_some_and(|mail| {
                mail.iter().any(|text| {
                    text.starts_with("Message from ")
                        || text.starts_with("Answer to question ")
                        || text.starts_with("The user resolved scope request ")
                })
            })
            || self.scope_service_needed("root")
    }
}

impl Session {
    pub(crate) fn mail_dialog_open(&self) -> bool {
        if !self.control.approvals.lock().unwrap().is_empty()
            || !self.control.fork_busy.lock().unwrap().is_empty()
            || !self.pending_reviewer_guidance().is_empty()
        {
            return true;
        }
        let j = self.control.journal.lock().unwrap();
        j.read_only
            || j.is_closed()
            || j.questions
                .values()
                .any(|q| (q.answer.is_none() && q.recipient.is_none()) || q.discussing)
            || j.contracts.iter().any(|(agent, c)| {
                (agent == "root" && c.roadblock().is_some())
                    || c.proposals.iter().any(|p| {
                        p.resolution.is_none()
                            && c.criteria().any(|criterion| {
                                criterion.id == p.criterion_id && criterion.owner == "user"
                            })
                    })
            })
            || j.header.goal.as_ref().is_some_and(|g| g.phase == "draft")
            || j.scope_requests
                .values()
                .any(|r| r.status == "pending" && r.escalation.is_some())
    }

    /// Whether deliverable root mail or an unserviced root scope request remains.
    pub fn has_pending_mail(&self) -> bool {
        self.control.root_mail_pending()
    }

    /// Whether a user cancel suspended automatic root mail turns (D134):
    /// mail waits until the user sends the next message. Survives resume.
    pub fn root_mail_suspended(&self) -> bool {
        self.coordinator.lock().unwrap().mail_suspended
            || self.control.journal.lock().unwrap().root_mail_suspended
    }

    /// Wait for root/background work and deliverable mail to settle. A user cancel
    /// returns promptly; shutdown remains the caller's responsibility.
    pub async fn wait_until_idle(&self) -> Result<(), EngineError> {
        loop {
            let changed = self.control.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (cancelled, inactive) = {
                let coordinator = self.coordinator.lock().unwrap();
                (coordinator.mail_suspended, coordinator.active.is_none())
            };
            let cancelled =
                cancelled || (inactive && self.control.journal.lock().unwrap().root_mail_suspended);
            if cancelled
                || (inactive
                    && !self.control.runtime.has_unfinished()
                    && !self.has_pending_mail()
                    && !self.control.has_queued("root", true))
            {
                return Ok(());
            }
            changed.await;
        }
    }

    pub(crate) fn watch_mail_turns(&self) {
        // Hold only Control while waiting. SessionLifetime must remain droppable
        // when the last frontend clone disappears.
        let control = self.control.clone();
        let lifetime = Arc::downgrade(&self._lifetime);
        let state = Arc::downgrade(&self.state);
        let gate = Arc::downgrade(&self.gate);
        let directories = self.directories.clone();
        tokio::spawn(async move {
            loop {
                let changed = control.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if control.health_cancel.is_cancelled() || lifetime.strong_count() == 0 {
                    break;
                }
                if control.root_mail_pending() {
                    // A fixed window batches arrivals without starving delivery
                    // when one child keeps sending updates.
                    sleep(Duration::from_millis(75)).await;
                    let (Some(lifetime), Some(state), Some(gate)) =
                        (lifetime.upgrade(), state.upgrade(), gate.upgrade())
                    else {
                        break;
                    };
                    let session = Session {
                        directories: directories.clone(),
                        state,
                        gate,
                        config: control.config.clone(),
                        control: control.clone(),
                        coordinator: lifetime.coordinator.clone(),
                        _lifetime: lifetime,
                    };
                    let mut turn = session.launch(Action::Mail);
                    tokio::spawn(async move { while turn.recv().await.is_some() {} });
                }
                tokio::select! {
                    _ = &mut changed => {},
                    _ = control.health_cancel.cancelled() => break,
                }
            }
        });
    }
}
