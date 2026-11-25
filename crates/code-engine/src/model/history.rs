//! Rendering conversation history into each provider wire's message items.
use super::Result;
use crate::schema::*;
use code_config::ProviderType;
use serde_json::{Value, json};

pub(super) fn chat_history(
    system: &str,
    history: &[HistoryEvent],
    assets: Option<&std::path::Path>,
) -> Result<Value> {
    let mut out = vec![json!({"role":"system","content":system})];
    for h in history {
        match h {
            HistoryEvent::ToolMedia { id, inputs } => {
                let mut content = vec![
                    json!({"type":"text","text":format!("Media returned by tool call {id}:")}),
                ];
                content.extend(crate::attachments::media_blocks(
                    ProviderType::OpenaiChat,
                    assets,
                    inputs,
                )?);
                out.push(json!({"role":"user","content":content}));
            }
            HistoryEvent::User { text, attachments } => {
                let mut content = vec![json!({"type":"text","text":text})];
                content.extend(crate::attachments::blocks(
                    ProviderType::OpenaiChat,
                    assets,
                    attachments,
                )?);
                out.push(json!({"role":"user","content":if attachments.is_empty() {json!(text)} else {json!(content)}}));
            }
            HistoryEvent::SystemReminder { text } | HistoryEvent::Compaction { summary: text } => {
                out.push(json!({"role":"user","content":text}))
            }
            HistoryEvent::Assistant {
                text,
                reasoning,
                tool_calls,
                ..
            } => {
                let mut m = json!({"role":"assistant","content":text});
                if !tool_calls.is_empty() {
                    m["tool_calls"] = json!(tool_calls.iter().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.replay_arguments().to_string()}})).collect::<Vec<_>>());
                }
                let details: Vec<_> = reasoning.iter().flat_map(|r| r.details.clone()).collect();
                if !details.is_empty() {
                    m["reasoning_details"] = json!(details);
                }
                let reason: String = reasoning.iter().map(|r| r.text.as_str()).collect();
                if !reason.is_empty() {
                    let field = reasoning
                        .iter()
                        .find_map(|r| r.field.as_deref())
                        .unwrap_or("reasoning_content");
                    m[field] = json!(reason);
                }
                out.push(m);
            }
            HistoryEvent::ToolResult { id, output, .. } => {
                out.push(json!({"role":"tool","tool_call_id":id,"content":output}))
            }
            _ => {}
        }
    }
    Ok(json!(out))
}
pub(super) fn anthropic_history(
    history: &[HistoryEvent],
    cache: Option<Value>,
    assets: Option<&std::path::Path>,
) -> Result<Value> {
    let mut out: Vec<Value> = vec![];
    for h in history {
        let (role, blocks) = match h {
            HistoryEvent::ToolMedia { id, inputs } => {
                let mut content = vec![
                    json!({"type":"text","text":format!("Media returned by tool call {id}:")}),
                ];
                content.extend(crate::attachments::media_blocks(
                    ProviderType::Anthropic,
                    assets,
                    inputs,
                )?);
                ("user", content)
            }
            HistoryEvent::User { text, attachments } => {
                let mut content = vec![json!({"type":"text","text":text})];
                content.extend(crate::attachments::blocks(
                    ProviderType::Anthropic,
                    assets,
                    attachments,
                )?);
                ("user", content)
            }
            HistoryEvent::SystemReminder { text } | HistoryEvent::Compaction { summary: text } => {
                ("user", vec![json!({"type":"text","text":text})])
            }
            HistoryEvent::Assistant {
                text,
                reasoning,
                tool_calls,
                replay,
                ..
            } => {
                if let Some(native) = replay
                    .as_ref()
                    .filter(|r| r.wire == ProviderType::Anthropic && !r.items.is_empty())
                {
                    out.push(json!({"role":"assistant","content":native.items}));
                    continue;
                }
                let mut blocks = vec![];
                for r in reasoning {
                    if let Some(sig) = &r.signature {
                        blocks.push(json!({"type":"thinking","thinking":r.text,"signature":sig}));
                    }
                    for d in &r.details {
                        if d["type"] == "redacted_thinking" {
                            blocks.push(d.clone());
                        }
                    }
                }
                if !text.is_empty() {
                    blocks.push(json!({"type":"text","text":text}));
                }
                for c in tool_calls {
                    blocks.push(
                        json!({"type":"tool_use","id":c.id,"name":c.name,"input":c.replay_arguments()}),
                    );
                }
                if blocks.is_empty() {
                    continue;
                }
                ("assistant", blocks)
            }
            HistoryEvent::ToolResult {
                id,
                output,
                is_error,
                ..
            } => (
                "user",
                vec![
                    json!({"type":"tool_result","tool_use_id":id,"content":output,"is_error":is_error}),
                ],
            ),
            _ => continue,
        };
        if let Some(last) = out.last_mut().filter(|m| m["role"] == role) {
            last["content"].as_array_mut().unwrap().extend(blocks);
        } else {
            out.push(json!({"role":role,"content":blocks}));
        }
    }
    if let (Some(last), Some(c)) = (out.last_mut(), cache)
        && let Some(block) = last["content"].as_array_mut().and_then(|a| a.last_mut())
    {
        block["cache_control"] = c;
    }
    Ok(json!(out))
}
fn response_call_id(id: &str) -> String {
    if id.starts_with("call_") {
        id.into()
    } else {
        format!("call_{}", id.trim_start_matches("fc_"))
    }
}
pub(super) fn responses_history(
    history: &[HistoryEvent],
    assets: Option<&std::path::Path>,
) -> Result<Value> {
    let mut out = vec![];
    for h in history {
        match h {
        HistoryEvent::ToolMedia { id, inputs } => {
            let mut content = vec![json!({"type":"input_text","text":format!("Media returned by tool call {id}:")})];
            content.extend(crate::attachments::media_blocks(ProviderType::OpenaiResponses, assets, inputs)?);
            out.push(json!({"role":"user","content":content}));
        },
        HistoryEvent::User { text, attachments } => {
            let mut content = vec![json!({"type":"input_text","text":text})];
            content.extend(crate::attachments::blocks(ProviderType::OpenaiResponses, assets, attachments)?);
            out.push(json!({"role":"user","content":if attachments.is_empty() {json!(text)} else {json!(content)}}));
        },
        HistoryEvent::SystemReminder {text} | HistoryEvent::Compaction {summary:text} => out.push(json!({"role":"user","content":text})),
        HistoryEvent::Assistant {text,reasoning,tool_calls,replay,..} => {
            if let Some(native) = replay.as_ref().filter(|r| r.wire == ProviderType::OpenaiResponses && !r.items.is_empty()) { out.extend(native.items.clone()); continue; }
            for r in reasoning { for d in &r.details { if d["type"] == "reasoning" { out.push(d.clone()); } } }
            if !text.is_empty() { out.push(json!({"role":"assistant","content":[{"type":"output_text","text":text}]})); }
            for c in tool_calls { out.push(json!({"type":"function_call","call_id":response_call_id(&c.id),"name":c.name,"arguments":c.replay_arguments().to_string()})); }
        },
        HistoryEvent::ToolResult {id,output,..} => out.push(json!({"type":"function_call_output","call_id":response_call_id(id),"output":output})),
        _ => {}
    }
    }
    Ok(json!(out))
}
