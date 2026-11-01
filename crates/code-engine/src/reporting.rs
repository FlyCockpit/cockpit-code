//! Durable partial reports and boundary-only notices of subtree growth.
use crate::{Control, schema::*, tools::ToolDefinition};
use serde_json::{Value, json};

#[derive(Clone)]
pub(crate) struct GrowthNotice {
    pub launch_id: String,
    pub agent_id: String,
    pub title: String,
    pub model: Option<String>,
    pub started_by: String,
    pub depth: usize,
}

pub(crate) fn partial_prefix(agent: &str) -> String {
    format!("Message from {agent}: Partial report: ")
}

pub(crate) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "report".into(),
        description: "Only for long-running children: send useful mid-run progress to your parent with partial:true and text. Short tasks should finish with a final answer. Replaces your previous pending partial; an idle root wakes. The latest partial is retained in automatic progress if you stop or fail. Continue working after this call; write the final report as your final answer.".into(),
        schema: json!({"type":"object","properties":{"partial":{"type":"boolean","const":true},"text":{"type":"string","minLength":1,"maxLength":16384}},"required":["partial","text"],"additionalProperties":false}),
    }
}

impl Control {
    pub(crate) fn partial_report(&self, agent: &str, args: &Value) -> Result<String, EngineError> {
        if self.flat_mode() {
            return Err(EngineError::bad(
                "partial reports are unavailable in flat mode",
            ));
        }
        let text = args["text"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.chars().count() <= 16384)
            .ok_or_else(|| EngineError::bad("report text must contain 1–16384 characters"))?;
        if args["partial"] != true {
            return Err(EngineError::bad(
                "report requires partial:true; write your final report as your final answer",
            ));
        }
        let parent = self
            .journal
            .lock()
            .unwrap()
            .agents
            .get(agent)
            .and_then(|a| a.parent_id.clone())
            .ok_or_else(|| EngineError::bad("only children can report to a parent"))?;
        let event = HistoryEvent::AgentPartialReport {
            agent_id: agent.into(),
            text: text.into(),
        };
        self.commit(agent, event)?;
        self.emit(EngineEvent::AgentMessage {
            from: agent.into(),
            to: parent.clone(),
            text: format!("Partial report: {text}"),
            breadcrumb: self.breadcrumb(&parent),
        });
        self.changed.notify_waiters();
        Ok(json!({"sent":true,"partial":true,"to":parent}).to_string())
    }

    pub(crate) fn deliver_growth(&self, agent: &str) -> Result<(), EngineError> {
        if self.flat_mode() {
            return Ok(());
        }
        let notices = self
            .journal
            .lock()
            .unwrap()
            .growth_pending
            .get(agent)
            .map(|notices| notices.iter().take(64).cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if notices.is_empty() {
            return Ok(());
        }
        let rows = notices
            .iter()
            .map(|n| {
                format!(
                    "- {} [{}] model={} startedBy={} depth={}",
                    n.title,
                    n.agent_id,
                    n.model.as_deref().unwrap_or("inherited"),
                    n.started_by,
                    n.depth,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Append first: a crash can repeat a notice but cannot lose it.
        self.commit(
            agent,
            HistoryEvent::SystemReminder {
                text: format!("New descendants in your subtree:\n{rows}"),
            },
        )?;
        self.commit(
            agent,
            HistoryEvent::SubtreeGrowthDelivered {
                launches: notices.into_iter().map(|n| n.launch_id).collect(),
            },
        )?;
        Ok(())
    }
}
