//! The engine's event stream and journal as UI events and replay items.
use super::convert::{
    change_views, error_kind, guidance_sel, guidance_view, job_view, prune_view, scope_view,
    structured_items, usage_view, user_wire,
};
use super::text::{strip_ctx, strip_guidance, summary_text};
use super::views::{AgentView, ApprovalView, EventRx, LocalRun, QuestionView, ReplayItem, UiEvent};
use super::{goal, terminal, tree};
use code_engine::Session;
use code_engine::schema::{EngineEvent, EventEnvelope, HistoryEvent};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub(super) fn replay_items(
    agent: &str,
    envelopes: &[EventEnvelope],
    assets: &Path,
) -> Vec<ReplayItem> {
    let mut out = vec![];
    let mut spoke = false;
    for e in envelopes {
        match &e.event {
            HistoryEvent::User { text, attachments } => {
                spoke = true;
                let text = user_wire(text, attachments, assets);
                out.push(ReplayItem::User(text, e.ts.clone()));
            }
            HistoryEvent::Assistant {
                text,
                reasoning,
                tool_calls,
                ..
            } => {
                spoke = true;
                let reasoning = reasoning
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() || !reasoning.trim().is_empty() {
                    out.push(ReplayItem::Assistant {
                        text: text.clone(),
                        reasoning,
                        ts: e.ts.clone(),
                    });
                }
                for c in tool_calls {
                    out.push(ReplayItem::ToolCall {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        args: c.arguments.clone(),
                    });
                }
            }
            HistoryEvent::ScriptCall {
                parent_call_id,
                call,
                result,
            } => out.push(ReplayItem::ScriptCall {
                parent: parent_call_id.clone(),
                call: call.clone(),
                result: result.clone().map(|mut r| {
                    r.output = strip_guidance(strip_ctx(&r.output));
                    r
                }),
            }),
            HistoryEvent::ToolResult {
                id,
                output,
                is_error,
                details,
                ..
            } => out.push(ReplayItem::ToolResult {
                id: id.clone(),
                output: strip_guidance(strip_ctx(output)),
                is_error: *is_error,
                details: details.clone(),
            }),
            HistoryEvent::TurnError { error } => {
                out.push(ReplayItem::Error(match error_kind(&error.kind) {
                    "error" => error.message.clone(),
                    k => format!("{k}: {}", error.message),
                }))
            }
            // The leading summary of a context; a trailing one duplicates the next context's.
            HistoryEvent::Compaction { summary } if !spoke => {
                out.push(ReplayItem::Compacted(summary_text(summary)))
            }
            HistoryEvent::QuestionsAnswered { answers, .. } => out.push(ReplayItem::Notice(
                format!("answered: {}", serde_json::to_string(answers).unwrap()),
            )),
            HistoryEvent::ModelSwitch { from, to, reason } => out.push(ReplayItem::Switched {
                from: from.clone(),
                to: to.clone(),
                reason: reason.clone(),
            }),
            HistoryEvent::CallUpdate { call, reason } => {
                if let Some(why) = reason
                    .strip_prefix("moved to background (")
                    .and_then(|r| r.strip_suffix(')'))
                {
                    out.push(ReplayItem::Moved {
                        id: call.id.clone(),
                        reason: why.to_string(),
                    })
                }
            }
            HistoryEvent::QuestionForkAnswered { id, summary, .. } => {
                out.push(ReplayItem::ForkAnswered {
                    id: id.clone(),
                    summary: summary.clone(),
                })
            }
            HistoryEvent::Message { from, to, text } if to == agent && from != "jobs" => {
                out.push(ReplayItem::Message {
                    from: from.clone(),
                    to: to.clone(),
                    text: strip_ctx(text),
                })
            }
            HistoryEvent::JobUpdate { job } if !job.running() => {
                out.push(ReplayItem::Job(job_view(job.clone())))
            }
            HistoryEvent::ScopeRequest { request } => {
                out.push(ReplayItem::Scope(scope_view(request.clone())))
            }
            HistoryEvent::SubscriptionNotice {
                changes, overflow, ..
            } => out.push(ReplayItem::Subscription {
                changes: change_views(changes.clone()),
                overflow: *overflow,
            }),
            HistoryEvent::GuidanceSelected { selection } => {
                out.push(ReplayItem::Guidance(guidance_sel(selection.clone())))
            }
            HistoryEvent::Pruned { report, .. } => {
                out.push(ReplayItem::Pruned(prune_view(report.clone())))
            }
            HistoryEvent::LocalCommand {
                command,
                exit_code,
                duration_ms,
                output_bytes,
            } => out.push(ReplayItem::LocalRun(LocalRun {
                command: command.clone(),
                exit_code: *exit_code,
                duration_ms: *duration_ms,
                output_bytes: *output_bytes,
            })),
            other => {
                if let Some((pct, used, window)) = tree::compact_suggestion(other) {
                    out.push(ReplayItem::CompactSuggested(pct, used, window));
                } else if let Some(row) = tree::tree_row(other) {
                    out.push(ReplayItem::Tree(row));
                }
            }
        }
    }
    out
}

/// Translate the long-lived session subscription into frontend events.
pub(super) fn subscribe(session: &Session, assets: PathBuf) -> EventRx {
    let mut rx = session.subscribe();
    let health = session.clone();
    tokio::spawn(async move {
        health.mcp_health_check().await;
    });
    let (tx, out) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut compacted_turn = None;
        while let Some(event) = rx.recv().await {
            if let EngineEvent::Committed { envelope } = &event
                && envelope.agent_id == "root"
            {
                match &envelope.event {
                    HistoryEvent::Compaction { .. } => {
                        compacted_turn = Some(envelope.turn_id.clone());
                    }
                    HistoryEvent::User { text, attachments }
                        if compacted_turn.as_ref() != Some(&envelope.turn_id) =>
                    {
                        let text = user_wire(text, attachments, &assets);
                        if tx.send(UiEvent::UserRecorded(text)).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(event) = translate(event)
                && tx.send(event).is_err()
            {
                break;
            }
        }
    });
    out
}

pub(super) fn translate(event: EngineEvent) -> Option<UiEvent> {
    Some(match event {
        EngineEvent::DecisionUpdated { open_count, .. } => UiEvent::Decision { open_count },
        EngineEvent::GuidanceSelected { selection } => UiEvent::Guidance(guidance_sel(selection)),
        EngineEvent::SubscriptionNotice {
            agent_id,
            changes,
            overflow,
        } => UiEvent::Subscription {
            agent: agent_id,
            changes: change_views(changes),
            overflow,
        },
        EngineEvent::ScopeRequestUpdate { request } => UiEvent::Scope(scope_view(request)),
        EngineEvent::ForkChat {
            fork_id,
            kind,
            text,
            messages,
            tool_call,
            tool_result,
            ..
        } => UiEvent::ForkChat {
            fork: fork_id,
            kind,
            text,
            messages,
            call: tool_call,
            result: tool_result,
        },
        EngineEvent::ReviewerGuidanceTrustRequest { request } => {
            UiEvent::GuidanceTrust(guidance_view(request))
        }
        EngineEvent::RootCompactionSuggested { used, window, .. } => {
            UiEvent::RootCompactionSuggested { used, window }
        }
        EngineEvent::IdleCompacted => UiEvent::IdleCompacted,
        EngineEvent::DiscoveryUpdated { .. } => UiEvent::DiscoveryUpdated,
        EngineEvent::GoalDraft { contract } => UiEvent::GoalDraft(goal::contract(&contract)),
        EngineEvent::GoalUpdate { .. } => UiEvent::GoalUpdate,
        EngineEvent::ProposalRequest { agent_id, proposal } => UiEvent::ProposalRequest {
            agent: agent_id,
            proposal: goal::proposal(&proposal),
        },
        EngineEvent::RoadblockRequest {
            agent_id,
            roadblock,
        } => UiEvent::RoadblockRequest {
            agent: agent_id,
            roadblock: goal::roadblock(&roadblock),
        },
        EngineEvent::Text { delta } => UiEvent::Text(delta),
        EngineEvent::ReplyEnd => UiEvent::ReplyEnd,
        EngineEvent::Reasoning { delta } => UiEvent::Reasoning(delta),
        EngineEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } => UiEvent::ToolDelta {
            index,
            id,
            name,
            args: arguments,
        },
        // Children's reviews arrive wrapped in AgentEvent (routed below).
        EngineEvent::SafetyReview {
            call_id,
            model,
            phase,
            verdict,
            reason,
            ..
        } => UiEvent::SafetyReview {
            call_id,
            model,
            result: (phase != "started").then(|| {
                let verdict = verdict.unwrap_or_else(|| "error".into());
                (verdict == "safe", verdict, reason.unwrap_or_default())
            }),
        },
        EngineEvent::ToolStart { call } => UiEvent::ToolStart {
            id: call.id,
            name: call.name,
            args: call.arguments,
        },
        EngineEvent::ToolEnd {
            id,
            output,
            is_error,
            details,
            ..
        } => UiEvent::ToolEnd {
            id,
            output: strip_guidance(strip_ctx(&output)),
            is_error,
            details,
        },
        EngineEvent::MovedToBackground {
            call_id,
            job_id,
            reason,
        } => UiEvent::MovedToBackground {
            call_id,
            job: job_id,
            reason,
        },
        EngineEvent::QueueChanged => UiEvent::QueueChanged,
        EngineEvent::QueuedMessageDelivered { message } => UiEvent::Message {
            from: "user".into(),
            to: message.agent,
            text: strip_ctx(&message.text),
        },
        EngineEvent::Usage { usage } => {
            if usage.partial {
                return None;
            }
            UiEvent::Usage(usage_view(&usage))
        }
        EngineEvent::Pruned { report } => UiEvent::Pruned(prune_view(report)),
        EngineEvent::Reconnecting { attempt, delay_ms } => {
            UiEvent::Reconnecting { attempt, delay_ms }
        }
        EngineEvent::ModelSwitched { from, to, reason } => {
            UiEvent::ModelSwitched { from, to, reason }
        }
        EngineEvent::Retry {
            attempt,
            delay_ms,
            reason,
        } => UiEvent::Retry {
            attempt,
            delay_ms,
            reason,
        },
        EngineEvent::StreamReset { .. } => UiEvent::StreamReset,
        EngineEvent::Error { error } => UiEvent::Error {
            kind: error_kind(&error.kind),
            message: error.message,
        },
        EngineEvent::Context { meter } => UiEvent::Context {
            used: meter.used,
            window: meter.window,
            reserved: meter.output_reservation,
            warning: meter.warning,
            known: meter.source != "unknown",
            estimate: meter.source.contains("tokenizer"),
        },
        EngineEvent::ApprovalRequest { request } => UiEvent::Approval(ApprovalView {
            call_id: request.call_id,
            parent_call_id: request.parent_call_id,
            id: request.id,
            agent: request.agent_id,
            job: request.job_id,
            tool: request.tool,
            breadcrumb: request.breadcrumb,
            target: request.target,
            diff: request.diff,
            command: request.command,
            reason: request.reason,
            grant_key: request.grant_key,
            verdict: request.sidecar_verdict.map(|v| {
                let why = request
                    .sidecar_reason
                    .into_iter()
                    .chain(request.sidecar_hint)
                    .collect::<Vec<_>>()
                    .join(" · ");
                (v, why)
            }),
        }),
        EngineEvent::Question { question } => {
            if question.recipient.is_some() {
                return None;
            }
            UiEvent::Question(QuestionView {
                questions: structured_items(&question),
                id: question.id,
                asker: question.asker,
                breadcrumb: question.breadcrumb,
                note: question.note,
                blocking: question.blocking,
            })
        }
        EngineEvent::SessionClaimed { message, .. } => UiEvent::Claimed(message),
        EngineEvent::PartialCompletion { native_reason, .. } if native_reason == "interrupted" => {
            UiEvent::Interrupted
        }
        EngineEvent::PartialCompletion { status, text, .. } => UiEvent::Partial {
            status: status.as_str().to_string(),
            text,
        },
        EngineEvent::Committed { envelope } => match envelope.event {
            HistoryEvent::QuestionsAnswered { id, .. } | HistoryEvent::QuestionDiscussed { id } => {
                UiEvent::Answered(id)
            }
            HistoryEvent::QuestionForkAnswered { id, summary, .. } => UiEvent::ForkAnswered {
                id,
                asker: envelope.agent_id,
                summary,
            },
            HistoryEvent::Goal { .. } => UiEvent::ContractChanged(envelope.agent_id),
            HistoryEvent::Compaction { summary } if envelope.agent_id == "root" => {
                UiEvent::Compacted(summary_text(&summary))
            }
            HistoryEvent::Compaction { summary } => UiEvent::Child {
                id: envelope.agent_id,
                event: Box::new(UiEvent::Compacted(summary_text(&summary))),
            },
            HistoryEvent::SettingChange { .. } => UiEvent::SettingsChanged,
            ref other => UiEvent::Tree {
                row: tree::tree_row(other)?,
                agent: envelope.agent_id,
            },
        },
        EngineEvent::TurnEnd { stop_reason } => UiEvent::TurnEnd { stop_reason },
        EngineEvent::AgentStarted {
            agent,
            execution,
            task_call_id,
            ..
        } => UiEvent::AgentStarted(AgentView {
            id: agent.agent_id,
            parent: agent.parent_id.unwrap_or_else(|| "root".into()),
            role: agent.agent,
            model: agent.model,
            status: agent.status,
            task: execution.task,
            background: execution.background,
            created: agent.created_at,
            call_id: task_call_id,
        }),
        EngineEvent::AgentStatus {
            agent_id, status, ..
        } => UiEvent::AgentStatus {
            id: agent_id,
            status,
        },
        EngineEvent::AgentFinished {
            agent_id, status, ..
        } => UiEvent::AgentFinished {
            id: agent_id,
            status,
        },
        // Terminal metadata is top-level whoever asked.
        EngineEvent::AgentEvent { event, .. }
            if matches!(*event, EngineEvent::TerminalChanged { .. }) =>
        {
            return translate(*event);
        }
        EngineEvent::AgentEvent {
            agent_id, event, ..
        } => UiEvent::Child {
            id: agent_id,
            event: Box::new(translate(*event)?),
        },
        // A partial report renders from its durable event (replaced in place).
        EngineEvent::AgentMessage { text, .. } if text.starts_with("Partial report: ") => {
            return None;
        }
        EngineEvent::AgentMessage { from, to, text, .. } => UiEvent::Message {
            from,
            to,
            text: strip_ctx(&text),
        },
        EngineEvent::Job { job } => UiEvent::Job(job_view(job)),
        EngineEvent::TerminalChanged { terminal } => {
            UiEvent::Terminal(terminal::term_info(terminal))
        }
        // Script progress gets its own rows in a later TUI lane.
        EngineEvent::ScriptCall {
            job_id,
            call,
            parent_call_id,
            result,
            status,
            ..
        } => UiEvent::ScriptCall {
            job: job_id,
            parent: parent_call_id,
            call,
            result: result.map(|mut r| {
                r.output = strip_guidance(strip_ctx(&r.output));
                r
            }),
            status,
        },
        EngineEvent::TitleChanged { title, source } => UiEvent::TitleChanged {
            title,
            auto: matches!(source, code_engine::schema::TitleSource::Auto),
        },
        EngineEvent::HiddenForkFinished { outcome } => UiEvent::HiddenFork {
            purpose: outcome.purpose,
            ok: outcome.succeeded,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_model_call_ends_the_reply_of_its_agent() {
        assert!(matches!(
            translate(EngineEvent::ReplyEnd),
            Some(UiEvent::ReplyEnd)
        ));
        let child = EngineEvent::AgentEvent {
            agent_id: "child".into(),
            breadcrumb: "root › child".into(),
            event: Box::new(EngineEvent::ReplyEnd),
        };
        assert!(matches!(
            translate(child),
            Some(UiEvent::Child { id, event }) if id == "child" && matches!(*event, UiEvent::ReplyEnd)
        ));
    }
}
