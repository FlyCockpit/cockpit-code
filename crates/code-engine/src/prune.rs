//! Explicit user action only. Canonical old journal records are never rewritten.
use crate::{Session, State, schema::*, sessions, tools::CommitBarrier};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub tokens_saved: u64,
    pub delegations: usize,
    pub tool_outputs: usize,
    pub tail_batches: usize,
    pub cache_break: bool,
    pub confirmation: String,
}
impl Session {
    /// Read-only preview for a frontend confirmation. Never invokes a model.
    pub fn prune_preview(&self) -> Result<PruneReport, EngineError> {
        if self.is_active() {
            return Err(EngineError::bad("wait for the active turn before pruning"));
        }
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| EngineError::bad("session is busy"))?;
        state.active_model = None;
        plan(&state).map(|(_, report)| report)
    }
    /// User-run only; intentionally absent from every model's offered tools.
    pub fn prune(&self) -> tokio::sync::mpsc::UnboundedReceiver<EngineEvent> {
        self.launch(crate::Action::Prune)
    }
}
fn plan(state: &State) -> Result<(Vec<HistoryEvent>, PruneReport), EngineError> {
    // A resumed Session has not run boundary_notices yet. Restore its frozen
    // prompt into a planning clone so preview neither writes nor changes the
    // live boundary-observation state.
    let mut planner = state.for_script(None);
    planner.active_model = None;
    if planner.context_state.frozen.is_none() {
        planner.context_state.frozen = planner.history().iter().rev().find_map(|e| match e {
            HistoryEvent::PromptContext { context } => Some(*context.clone()),
            _ => None,
        });
    }
    let state = &planner;
    let original = state.history();
    let mut history = original.clone();
    let model = state.model()?;
    let definitions = state.definitions();
    let before = state
        .prepare(&model, &definitions, &original)?
        .input_estimate;
    let finished = {
        let j = state.control.journal.lock().unwrap();
        let mut tasks = BTreeMap::new();
        if j.is_persisted() {
            for (agent, contexts) in sessions::read_contexts(&j.dir)? {
                let Some(record) = j.agents.get(&agent).filter(|a| {
                    matches!(
                        a.status.as_str(),
                        "done" | "failed" | "killed" | "roadblocked"
                    )
                }) else {
                    continue;
                };
                for envelope in contexts.into_values().flatten() {
                    if let HistoryEvent::AgentStarted {
                        task_call_id: Some(id),
                        ..
                    } = envelope.event
                    {
                        tasks.insert(id, format!("{} ({})", record.agent_id, record.status));
                    }
                }
            }
        }
        tasks
    };
    let mut stubs = BTreeMap::new();
    for event in &history {
        if let HistoryEvent::Assistant { tool_calls, .. } = event {
            for call in tool_calls {
                if let Some(child) = finished.get(&call.id)
                    && let Some(prompt) = call.arguments["prompt"].as_str()
                {
                    let stub = format!(
                        "[Finished delegation to {child}; prompt pruned by user. Full prompt: history search/read, call {}.]",
                        call.id
                    );
                    if prompt.len() > 512 && stub.len() < prompt.len() {
                        stubs.insert(call.id.clone(), stub);
                    }
                }
            }
        }
    }
    // Monty stores delegation arguments inside the outer script source. Replace
    // only exact quoted literals backed by a completed ScriptCall, never a
    // computed expression or arbitrary source region.
    let mut unfinished_prompts = BTreeSet::new();
    for event in &history {
        let calls = match event {
            HistoryEvent::Assistant { tool_calls, .. } => tool_calls.iter().collect::<Vec<_>>(),
            HistoryEvent::ScriptCall { call, .. } => vec![call],
            _ => vec![],
        };
        for call in calls {
            if !finished.contains_key(&call.id)
                && let Some(prompt) = call.arguments["prompt"].as_str()
            {
                unfinished_prompts.insert(prompt.to_owned());
            }
        }
    }
    let mut code_updates = BTreeMap::new();
    let mut changed_delegations: BTreeSet<_> = stubs.keys().cloned().collect();
    for event in &history {
        if let HistoryEvent::ScriptCall {
            parent_call_id: Some(parent),
            call,
            result: Some(result),
        } = event
            && !result.is_error
            && let Some(child) = finished.get(&call.id)
            && let Some(prompt) = call.arguments["prompt"]
                .as_str()
                .filter(|s| s.len() > 512 && !unfinished_prompts.contains(*s))
        {
            let original_code = history.iter().find_map(|e| match e {
                HistoryEvent::Assistant { tool_calls, .. } => tool_calls
                    .iter()
                    .find(|c| &c.id == parent)
                    .and_then(|c| c.arguments["code"].as_str()),
                _ => None,
            });
            let Some(original_code) = original_code else {
                continue;
            };
            let code = code_updates
                .get(parent)
                .cloned()
                .unwrap_or_else(|| original_code.to_owned());
            let stub = format!(
                "[Finished delegation to {child}; prompt pruned by user. Full prompt: history search/read, call {}.]",
                call.id
            );
            let replacement = serde_json::to_string(&stub).unwrap();
            let single = format!(
                "'{}'",
                prompt
                    .replace('\\', "\\\\")
                    .replace('\'', "\\'")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t")
            );
            let variants = [
                format!("\"\"\"{prompt}\"\"\""),
                format!("'''{prompt}'''"),
                serde_json::to_string(prompt).unwrap(),
                single,
            ];
            let mut revised = code.clone();
            for literal in variants {
                revised = revised.replace(&literal, &replacement);
            }
            if revised != code {
                code_updates.insert(parent.clone(), revised);
                changed_delegations.insert(call.id.clone());
            }
        }
    }
    let starts = history
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matches!(e, HistoryEvent::Assistant { .. }).then_some(i))
        .collect::<Vec<_>>();
    let tail = starts.iter().rev().nth(3).copied().unwrap_or(0);
    let mut outputs = BTreeSet::new();
    for (index, event) in history.iter_mut().enumerate() {
        let update = |call: &mut ToolCall| {
            if let Some(stub) = stubs.get(&call.id) {
                call.arguments["prompt"] = json!(stub);
            }
            if let Some(code) = code_updates.get(&call.id) {
                call.arguments["code"] = json!(code);
            }
        };
        match event {
            HistoryEvent::Assistant {
                tool_calls, replay, ..
            } => {
                for call in tool_calls.iter_mut() {
                    update(call);
                }
                if let Some(replay) = replay {
                    for item in &mut replay.items {
                        if let Some(call) = tool_calls.iter().find(|c| {
                            (stubs.contains_key(&c.id) || code_updates.contains_key(&c.id))
                                && (item["id"] == c.id || item["call_id"] == c.id)
                        }) {
                            match item["type"].as_str() {
                                Some("tool_use") => item["input"] = call.arguments.clone(),
                                Some("function_call") => {
                                    item["arguments"] = json!(call.arguments.to_string())
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            HistoryEvent::ToolCall { call } | HistoryEvent::CallUpdate { call, .. } => update(call),
            HistoryEvent::ToolResult {
                id,
                output,
                details,
                ..
            } if index < tail && (output.len() > 2048 || !details.media.is_empty()) => {
                *output = format!(
                    "[Tool output pruned by user: {} bytes, sha256 {}. Full output/media: history search/read, call {id}.]",
                    output.len(),
                    crate::context::digest(output.as_bytes())
                );
                details.media.clear();
                outputs.insert(id.clone());
            }
            _ => {}
        }
    }
    let after = state
        .prepare(&model, &definitions, &history)?
        .input_estimate;
    let saved = before.saturating_sub(after);
    let changed = saved > 0 && (!changed_delegations.is_empty() || !outputs.is_empty());
    let mut report = PruneReport {
        tokens_before: before,
        tokens_after: if changed { after } else { before },
        tokens_saved: saved,
        delegations: changed_delegations.len(),
        tool_outputs: outputs.len(),
        tail_batches: 4,
        cache_break: changed,
        confirmation: String::new(),
    };
    if !changed {
        history = original;
        report.tokens_saved = 0;
        report.delegations = 0;
        report.tool_outputs = 0;
    }
    report.confirmation = if changed {
        format!(
            "Prune saves about {} tokens: {} finished delegation prompts and {} old tool outputs. This deliberately breaks the prompt cache. The latest four assistant/tool batches, user messages and criteria stay intact; full originals remain in journal history.",
            report.tokens_saved, report.delegations, report.tool_outputs
        )
    } else {
        "No eligible history to prune; 0 tokens saved. Pruning deliberately breaks the prompt cache only when history changes.".into()
    };
    Ok((history, report))
}
pub(crate) fn apply(
    state: &mut State,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
) -> Result<String, EngineError> {
    state.control.check_owner()?;
    let (history, report) = plan(state)?;
    if cancel.is_cancelled() {
        return Err(EngineError::cancelled());
    }
    barrier
        .commit(|| {
            state.control.commit(
                &state.agent_id,
                HistoryEvent::Pruned {
                    history,
                    report: report.clone(),
                },
            )?;
            Ok(())
        })
        .map_err(|e| EngineError::bad(e.to_string()))?;
    if report.cache_break {
        state.anchor = None;
        state.continuing = false;
        state.warned_context = false;
        *state.control.last_root_request.lock().unwrap() = None;
        let model = state.model()?;
        let meter = ContextMeter {
            used: report.tokens_after,
            window: model.config.context_window,
            output_reservation: model.config.max_output_tokens.unwrap_or(8192),
            source: "tokenizer (after user prune)".into(),
            warning: false,
        };
        *state.control.meter.lock().unwrap() = meter.clone();
        state.control.emit(EngineEvent::Context { meter });
    }
    state.control.emit(EngineEvent::Pruned { report });
    Ok("pruned".into())
}
