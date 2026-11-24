//! Per-session render state: the tool-call cards shown to the client, review
//! notes and approvals that arrived before their call, the goal phase last
//! drawn into the plan, and which root `task` card each subagent reports to.
//! `AcpSession::event` translates engine events; this module owns what they
//! change and the `tool_call_update`s that follow from it.

use crate::mapping::{self, clip, text_content};
use crate::rpc::Out;
use code_engine::schema::{ApprovalRequest, EngineEvent};
use serde_json::{Value, json};
use std::collections::HashMap;

/// How long a root approval waits for its ToolStart before it gets a card of
/// its own.
const DEFER: std::time::Duration = std::time::Duration::from_millis(250);

struct CallView {
    base: Vec<Value>,
    notes: Vec<String>,
    output: Option<String>,
}

impl CallView {
    fn note(&mut self, line: String) {
        self.notes.push(line);
        if self.notes.len() > 12 {
            self.notes.remove(0);
        }
    }
    fn content(&self) -> Vec<Value> {
        let mut c = self.base.clone();
        if !self.notes.is_empty() {
            c.push(text_content(&self.notes.join("\n")));
        }
        if let Some(o) = &self.output {
            c.push(text_content(o));
        }
        c
    }
}

#[derive(Default)]
pub(crate) struct View {
    goal_phase: Option<(String, String)>,
    calls: HashMap<String, CallView>,
    /// Every subagent to the root `task` call that (transitively) spawned it.
    child_task: HashMap<String, String>,
    status: HashMap<String, String>,
    /// Review notes for calls whose ToolStart has not arrived yet.
    early_notes: HashMap<String, Vec<String>>,
    /// Root approvals waiting for their ToolStart, with a deadline.
    deferred: Vec<(ApprovalRequest, tokio::time::Instant)>,
}

impl View {
    /// Records the root goal's (disposition, phase); true when it changed and
    /// the plan should be redrawn.
    pub(crate) fn goal_phase_changed(&mut self, disposition: &str, phase: &str) -> bool {
        let next = (disposition.to_owned(), phase.to_owned());
        if self.goal_phase.as_ref() == Some(&next) {
            return false;
        }
        self.goal_phase = Some(next);
        true
    }

    /// Whether a card exists for this call.
    pub(crate) fn has_call(&self, id: &str) -> bool {
        self.calls.contains_key(id)
    }

    /// Opens the card for a root ToolStart whose `tool_call` was just sent:
    /// review notes that came first are shown on it, and the deferred
    /// approvals for this call are returned to be asked against it.
    pub(crate) fn tool_started(
        &mut self,
        out: &Out,
        sid: &str,
        id: &str,
        base: Vec<Value>,
    ) -> Vec<ApprovalRequest> {
        let call = CallView {
            base,
            notes: self.early_notes.remove(id).unwrap_or_default(),
            output: None,
        };
        if !call.notes.is_empty() {
            show(out, sid, id, &call);
        }
        let (matched, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred)
            .into_iter()
            .partition(|(r, _)| r.call_id == id);
        self.deferred = rest;
        self.calls.insert(id.to_owned(), call);
        matched.into_iter().map(|(request, _)| request).collect()
    }

    /// Opens the card for a root script sub-call whose `tool_call` was just
    /// sent.
    pub(crate) fn script_call_started(&mut self, id: String, base: Vec<Value>) {
        self.calls.insert(
            id,
            CallView {
                base,
                notes: vec![],
                output: None,
            },
        );
    }

    /// Records a call's final output, dropping its progress notes when the
    /// output replaces them; returns the card's content, or None when there
    /// is no card.
    pub(crate) fn finished(
        &mut self,
        id: &str,
        output: Option<String>,
        drop_notes: bool,
    ) -> Option<Vec<Value>> {
        let call = self.calls.get_mut(id)?;
        call.output = output;
        if drop_notes {
            call.notes.clear();
        }
        Some(call.content())
    }

    /// A safety-review note replaces the call's "Reviewing with" line; it is
    /// kept for later when the call has no card yet.
    pub(crate) fn review(&mut self, out: &Out, sid: &str, call_id: String, note: String) {
        let review = |notes: &mut Vec<String>| {
            notes.retain(|n| !n.starts_with("Reviewing with "));
            notes.push(note);
        };
        match self.calls.get_mut(&call_id) {
            None => review(self.early_notes.entry(call_id).or_default()),
            Some(call) => {
                review(&mut call.notes);
                show(out, sid, &call_id, call);
            }
        }
    }

    /// Holds a root approval until its ToolStart arrives or the deadline
    /// passes.
    pub(crate) fn defer(&mut self, request: ApprovalRequest) {
        self.deferred
            .push((request, tokio::time::Instant::now() + DEFER));
    }

    /// When the oldest deferred approval stops waiting.
    pub(crate) fn deferred_deadline(&self) -> Option<tokio::time::Instant> {
        self.deferred.first().map(|(_, at)| *at)
    }

    /// Every deferred approval, to be asked without a call.
    pub(crate) fn take_deferred(&mut self) -> Vec<ApprovalRequest> {
        std::mem::take(&mut self.deferred)
            .into_iter()
            .map(|(request, _)| request)
            .collect()
    }

    /// Adds a progress line to a call's card.
    pub(crate) fn note(&mut self, out: &Out, sid: &str, call_id: &str, line: String) {
        if let Some(call) = self.calls.get_mut(call_id) {
            call.note(line);
            show(out, sid, call_id, call);
        }
    }

    /// Adds a line to the `task` card of the subagent `agent`, if it has one.
    pub(crate) fn agent_note(&mut self, out: &Out, sid: &str, agent: &str, line: String) {
        if let Some(task) = self.child_task.get(agent).cloned() {
            self.note(out, sid, &task, line);
        }
    }

    /// Subagent activity, summarised under its root `task` call: start,
    /// status changes, tool titles, failures and the final report. Child
    /// text and reasoning streams are not forwarded.
    pub(crate) fn child(
        &mut self,
        out: &Out,
        sid: &str,
        agent: Option<(&str, &str)>,
        event: EngineEvent,
    ) {
        let task_of = |v: &Self, id: &str| v.child_task.get(id).cloned();
        match event {
            EngineEvent::AgentStarted {
                agent: record,
                execution,
                breadcrumb,
                task_call_id,
            } => {
                let parent = record.parent_id.clone().unwrap_or_else(|| "root".into());
                let task = if parent == "root" {
                    task_call_id
                } else {
                    task_of(self, &parent)
                };
                let Some(task) = task else { return };
                self.child_task
                    .insert(record.agent_id.clone(), task.clone());
                let line = format!(
                    "▸ {breadcrumb} started ({}{}){}",
                    record.model.as_deref().unwrap_or("default model"),
                    if execution.background {
                        ", background"
                    } else {
                        ""
                    },
                    if parent == "root" {
                        String::new()
                    } else {
                        format!(
                            ": {}",
                            clip(execution.task.lines().next().unwrap_or(""), 80)
                        )
                    }
                );
                self.note(out, sid, &task, line);
            }
            EngineEvent::AgentStatus {
                agent_id,
                status,
                breadcrumb,
                ..
            } => {
                if agent_id == "root" || self.status.get(&agent_id) == Some(&status) {
                    return;
                }
                self.status.insert(agent_id.clone(), status.clone());
                if let Some(task) = task_of(self, &agent_id) {
                    let shown = match status.strip_prefix("queued:") {
                        Some(n) => format!("queued ({n})"),
                        None => status,
                    };
                    self.note(out, sid, &task, format!("{breadcrumb}: {shown}"));
                }
            }
            EngineEvent::AgentFinished {
                agent_id,
                status,
                report,
                breadcrumb,
                ..
            } => {
                if let Some(task) = task_of(self, &agent_id) {
                    let line = format!(
                        "■ {breadcrumb} {status}: {}",
                        clip(report.trim().lines().next().unwrap_or(""), 200)
                    );
                    self.note(out, sid, &task, line);
                }
            }
            EngineEvent::AgentEvent {
                agent_id,
                breadcrumb,
                event,
            } => self.child(out, sid, Some((&agent_id, &breadcrumb)), *event),
            EngineEvent::Context { meter } if agent.is_some() => {
                let (id, breadcrumb) = agent.unwrap();
                if let Some(task) = task_of(self, id) {
                    // A child's context belongs to its task, never to the
                    // session-wide usage_update meter owned by the root.
                    out.update(
                        sid,
                        json!({
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": task,
                            "_meta": {"cockpit-code": {"contextMeter": {
                                "agentId": id, "breadcrumb": breadcrumb, "meter": meter
                            }}}
                        }),
                    );
                }
            }
            EngineEvent::ToolStart { call } if agent.is_some() => {
                let (id, crumb) = agent.unwrap();
                if let Some(task) = task_of(self, id) {
                    let line = format!(
                        "{crumb} › {}",
                        mapping::tool_title(&call.name, &call.arguments, None)
                    );
                    self.note(out, sid, &task, line);
                }
            }
            EngineEvent::ToolEnd {
                name,
                is_error: true,
                output,
                ..
            } if agent.is_some() => {
                let (id, crumb) = agent.unwrap();
                if let Some(task) = task_of(self, id) {
                    let line = format!(
                        "{crumb} ✗ {name}: {}",
                        clip(output.lines().next().unwrap_or(""), 120)
                    );
                    self.note(out, sid, &task, line);
                }
            }
            EngineEvent::SafetyReview {
                phase,
                verdict,
                model,
                ..
            } if agent.is_some() && phase != "started" => {
                let (id, crumb) = agent.unwrap();
                if let Some(task) = task_of(self, id) {
                    let line = format!(
                        "{crumb} safety review ({model}): {}",
                        verdict.as_deref().unwrap_or("error")
                    );
                    self.note(out, sid, &task, line);
                }
            }
            EngineEvent::Error { error } if agent.is_some() => {
                let (id, crumb) = agent.unwrap();
                if let Some(task) = task_of(self, id) {
                    self.note(
                        out,
                        sid,
                        &task,
                        format!("{crumb} error: {}", clip(&error.message, 200)),
                    );
                }
            }
            _ => {}
        }
    }
}

/// Sends a call card's current content.
fn show(out: &Out, sid: &str, call_id: &str, call: &CallView) {
    out.update(
        sid,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": call_id, "content": call.content()}),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn early_review_and_approval_wait_for_their_tool_start() {
        let (tx, mut frames) = tokio::sync::mpsc::unbounded_channel();
        let out = Out::new(tx);
        let mut view = View::default();
        view.review(&out, "s", "call".into(), "Reviewing with m…".into());
        view.review(&out, "s", "call".into(), "Safety review (m): allow ".into());
        assert!(frames.try_recv().is_err());
        let request: ApprovalRequest = serde_json::from_value(json!({
            "id":"a", "callId":"call", "agentId":"root", "breadcrumb":"root",
            "tool":"bash", "requested":{}, "resolved":{}, "reason":"approval"
        }))
        .unwrap();
        view.defer(request);
        assert!(view.deferred_deadline().is_some() && !view.has_call("call"));
        let matched = view.tool_started(&out, "s", "call", vec![]);
        assert_eq!(matched.len(), 1);
        assert!(view.deferred_deadline().is_none() && view.has_call("call"));
        let frame: Value = serde_json::from_str(&frames.try_recv().unwrap()).unwrap();
        assert_eq!(
            frame["params"]["update"]["content"][0]["content"]["text"],
            "Safety review (m): allow "
        );
        assert_eq!(
            view.finished("call", Some("done".into()), true)
                .unwrap()
                .len(),
            1
        );
    }
}
