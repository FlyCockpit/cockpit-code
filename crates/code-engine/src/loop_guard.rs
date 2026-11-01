//! Contiguous identical-call collapse, adapted from FlyCockpit's Cockpit.
use crate::schema::{HistoryEvent, ToolCall};
#[derive(Default)]
pub struct LoopGuard {
    last: String,
    count: u32,
    tools: String,
}
impl LoopGuard {
    pub fn new(definitions: &[crate::tools::ToolDefinition]) -> Self {
        Self {
            tools: definitions
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            ..Self::default()
        }
    }
    pub fn check(&mut self, call: &ToolCall) -> Option<String> {
        let signature = format!("{}:{}", call.name, call.arguments);
        if signature == self.last {
            self.count += 1;
        } else {
            self.last = signature;
            self.count = 1;
        }
        if self.count < 3 {
            return None;
        }
        let args: String = call.arguments.to_string().chars().take(160).collect();
        Some(format!(
            "Loop blocked: `{}` {args} was called {} times with the same arguments. Do not re-issue it — choose a different action. Available tools: {}. Consult their required arguments; do not repeat the rejected call.",
            call.name, self.count, self.tools
        ))
    }
    pub fn exhausted(&self) -> bool {
        self.count >= 8
    }
}
/// Collapse only previous *blocked* replay pairs, retaining usage and durable metadata.
pub fn collapse(history: &mut Vec<HistoryEvent>, call: &ToolCall) -> bool {
    let mut changed = false;
    loop {
        let projection: Vec<_> = history
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                matches!(
                    e,
                    HistoryEvent::User { .. }
                        | HistoryEvent::Assistant { .. }
                        | HistoryEvent::ToolResult { .. }
                        | HistoryEvent::SystemReminder { .. }
                        | HistoryEvent::Compaction { .. }
                )
                .then_some(i)
            })
            .collect();
        if projection.len() < 3 {
            return changed;
        }
        let [a, b, c] = projection[projection.len() - 3..] else {
            return changed;
        };
        let matches_call = |event: &HistoryEvent| matches!(event,HistoryEvent::Assistant{tool_calls,..}if tool_calls.len()==1&&tool_calls[0].name==call.name&&history.iter().find_map(|e|if let HistoryEvent::ToolCall{call:c}=e{(c.id==tool_calls[0].id).then_some(c)}else{None}).unwrap_or(&tool_calls[0]).arguments==call.arguments);
        if !matches_call(&history[a])
            || !matches_call(&history[c])
            || !matches!(&history[b],HistoryEvent::ToolResult{output,..}if output.starts_with("Loop blocked:"))
        {
            return changed;
        }
        history.remove(b);
        history.remove(a);
        changed = true;
    }
}
