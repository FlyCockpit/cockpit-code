//! Session-local frontend queue. Delivery is committed before an entry leaves the queue.
use crate::{Control, Session, State, schema::*};
use code_config::QueueClass;
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub id: u64,
    pub agent: String,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    pub class: QueueClass,
}
struct Entry {
    message: QueuedMessage,
    since: Instant,
}
#[derive(Default)]
pub(crate) struct Queue {
    next: u64,
    entries: Vec<Entry>,
}
impl Session {
    pub fn queue_message(
        &self,
        agent: &str,
        text: impl Into<String>,
        class: QueueClass,
    ) -> Result<u64, EngineError> {
        self.queue_message_with_attachments(agent, text, vec![], class)
    }
    pub fn queue_message_with_attachments(
        &self,
        agent: &str,
        text: impl Into<String>,
        attachments: Vec<Attachment>,
        class: QueueClass,
    ) -> Result<u64, EngineError> {
        self.control.check_owner()?;
        let agent = self.resolve_agent_id(agent)?;
        let agent = agent.as_str();
        if self.coordinator.lock().unwrap().closing {
            return Err(EngineError::bad("session is closing"));
        }
        if !self
            .control
            .journal
            .lock()
            .unwrap()
            .agents
            .contains_key(agent)
        {
            return Err(EngineError::bad("agent not found"));
        }
        if agent == "root" && class == QueueClass::Interrupt {
            self.control.pause_goal("user")?;
        }
        let (text, attachments) = self.import_attachment_tags(text.into(), attachments)?;
        let mut queue = self.control.queue.lock().unwrap();
        queue.next += 1;
        let id = queue.next;
        queue.entries.push(Entry {
            message: QueuedMessage {
                id,
                agent: agent.into(),
                text,
                attachments,
                class,
            },
            since: Instant::now(),
        });
        drop(queue);
        if class != QueueClass::NextTurn {
            self.resume_discussion(agent);
        }
        self.control.queue_changed();
        self.wake_queue_turn();
        Ok(id)
    }
    pub fn queued_messages(&self, agent: &str) -> Vec<QueuedMessage> {
        let agent = self
            .resolve_agent_id(agent)
            .unwrap_or_else(|_| agent.into());
        self.control
            .queue
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.message.agent == agent)
            .map(|e| e.message.clone())
            .collect()
    }
    pub fn reclassify(&self, id: u64, class: QueueClass) -> Result<(), EngineError> {
        self.control.check_owner()?;
        let mut queue = self.control.queue.lock().unwrap();
        let entry = queue
            .entries
            .iter_mut()
            .find(|e| e.message.id == id)
            .ok_or_else(|| EngineError::bad("that message was already sent"))?;
        // A promotion starts its wait when it first becomes steering.
        if entry.message.class == QueueClass::NextTurn && class != QueueClass::NextTurn {
            entry.since = Instant::now();
        }
        entry.message.class = class;
        let agent = entry.message.agent.clone();
        drop(queue);
        if agent == "root" && class == QueueClass::Interrupt {
            self.control.pause_goal("user")?;
        }
        if class != QueueClass::NextTurn {
            self.resume_discussion(&agent);
        }
        self.control.queue_changed();
        self.wake_queue_turn();
        Ok(())
    }
    pub fn unqueue(&self, id: u64) -> Result<Option<(QueuedMessage, usize)>, EngineError> {
        self.control.check_owner()?;
        let mut queue = self.control.queue.lock().unwrap();
        let Some(index) = queue.entries.iter().position(|e| e.message.id == id) else {
            return Ok(None);
        };
        let entry = queue.entries.remove(index);
        drop(queue);
        self.control.queue_changed();
        Ok(Some((entry.message, index)))
    }
    pub(crate) fn wake_queue_turn(&self) {
        if self.is_active() || !self.control.has_queued("root", true) {
            return;
        }
        let mut events = self.launch(crate::Action::Queued);
        tokio::spawn(async move { while events.recv().await.is_some() {} });
    }
}
impl Control {
    fn queue_changed(&self) {
        self.emit(EngineEvent::QueueChanged);
        self.changed.notify_waiters();
    }
    pub(crate) fn has_queued(&self, agent: &str, next_turn: bool) -> bool {
        self.queue.lock().unwrap().entries.iter().any(|e| {
            e.message.agent == agent && (next_turn || e.message.class != QueueClass::NextTurn)
        })
    }
    pub(crate) fn has_interrupt(&self, agent: &str) -> bool {
        self.queue
            .lock()
            .unwrap()
            .entries
            .iter()
            .any(|e| e.message.agent == agent && e.message.class == QueueClass::Interrupt)
    }
    pub(crate) fn background_deadline(
        &self,
        agent: &str,
        started: Instant,
        config: &code_config::Config,
    ) -> Option<(Instant, &'static str)> {
        if self.scope_service_needed(agent) {
            return Some((Instant::now(), "scope request"));
        }
        if self.has_interrupt(agent) {
            return Some((Instant::now(), "interrupt"));
        }
        let since = self
            .queue
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.message.agent == agent && e.message.class == QueueClass::Steer)
            .map(|e| e.since)
            .min();
        let mailbox = self.mail_since.lock().unwrap().get(agent).copied();
        since.into_iter().chain(mailbox).min().and_then(|since| {
            Some((
                std::cmp::max(
                    started.checked_add(Duration::from_secs(config.steer_background_secs))?,
                    since.checked_add(Duration::from_secs(config.steer_min_wait_secs))?,
                ),
                "steer",
            ))
        })
    }
    fn deliver_queue(&self, agent: &str, next_turn: bool) -> Result<(), EngineError> {
        let mut queue = self.queue.lock().unwrap();
        let mut changed = false;
        let mut index = 0;
        while index < queue.entries.len() {
            let m = &queue.entries[index].message;
            if m.agent != agent || (!next_turn && m.class == QueueClass::NextTurn) {
                index += 1;
                continue;
            }
            if self.discussing(agent) {
                self.commit(
                    agent,
                    HistoryEvent::DiscussionResumed {
                        asker: agent.into(),
                    },
                )?;
                self.changed.notify_waiters();
            }
            // User input is canonical at delivery, never while it is held/editable.
            self.commit(
                agent,
                HistoryEvent::User {
                    text: m.text.clone(),
                    attachments: m.attachments.clone(),
                },
            )?;
            self.emit(EngineEvent::QueuedMessageDelivered { message: m.clone() });
            queue.entries.remove(index);
            changed = true;
        }
        drop(queue);
        if changed {
            self.queue_changed();
        }
        Ok(())
    }
}
impl State {
    pub(crate) fn deliver_queue(&self, next_turn: bool) -> Result<(), EngineError> {
        self.control.deliver_queue(&self.agent_id, next_turn)
    }
}
