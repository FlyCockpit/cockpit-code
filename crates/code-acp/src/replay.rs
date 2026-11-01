//! `session/load` history replay: the root agent's contexts as
//! `session/update` notifications, oldest first.

use crate::mapping::{self, clip, text_block};
use crate::rpc::Out;
use base64::Engine as _;
use code_engine::Session;
use code_engine::schema::HistoryEvent;
use serde_json::{Value, json};
use std::collections::HashMap;

pub fn replay(out: &Out, sid: &str, session: &Session) {
    let Ok(snapshot) = session.snapshot() else {
        return;
    };
    let cwd = session.cwd();
    let Some(contexts) = snapshot.contexts.get("root") else {
        return;
    };
    let mut calls: HashMap<String, (String, Vec<Value>)> = HashMap::new();
    let mut todos = None;
    for envelopes in contexts.values() {
        let mut spoke = false;
        for e in envelopes {
            match &e.event {
                HistoryEvent::TurnError { error } => out.update(sid, json!({"sessionUpdate":"agent_message_chunk","content":text_block(&format!("Error: {}",error.message))})),
                HistoryEvent::User { text, attachments } => {
                    spoke = true;
                    out.update(
                        sid,
                        json!({"sessionUpdate": "user_message_chunk", "content": text_block(text)}),
                    );
                    for attachment in attachments {
                        if let Ok(bytes) = session.attachment_bytes(attachment) {
                            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                            let content = if attachment.mime_type.starts_with("image/") || attachment.mime_type.starts_with("audio/") {
                                json!({"type":if attachment.mime_type.starts_with("image/") {"image"} else {"audio"},"data":data,"mimeType":attachment.mime_type})
                            } else { json!({"type":"resource","resource":{"uri":format!("file:///assets/{}",attachment.path),"mimeType":attachment.mime_type,"blob":data}}) };
                            out.update(sid, json!({"sessionUpdate":"user_message_chunk","content":content}));
                        }
                    }
                }
                HistoryEvent::Assistant {
                    text,
                    reasoning,
                    tool_calls,
                    ..
                } => {
                    spoke = true;
                    let thought = reasoning
                        .iter()
                        .map(|r| r.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !thought.trim().is_empty() {
                        out.update(
                            sid,
                            json!({"sessionUpdate": "agent_thought_chunk", "content": text_block(&thought)}),
                        );
                    }
                    if !text.trim().is_empty() {
                        out.update(
                            sid,
                            json!({"sessionUpdate": "agent_message_chunk", "content": text_block(text)}),
                        );
                    }
                    for c in tool_calls {
                        if c.name == "todo" {
                            todos = Some(c.arguments["items"].clone());
                            continue;
                        }
                        let base = mapping::start_content(&c.name, &c.arguments, &cwd, None);
                        out.update(
                            sid,
                            json!({
                                "sessionUpdate": "tool_call", "toolCallId": c.id,
                                "title": mapping::tool_title(&c.name, &c.arguments, Some(&cwd)),
                                "kind": mapping::tool_kind(&c.name), "status": "in_progress",
                                "content": base, "rawInput": c.arguments,
                                "locations": mapping::locations(&c.name, &c.arguments, &cwd),
                            }),
                        );
                        calls.insert(c.id.clone(), (c.name.clone(), base));
                    }
                }
                HistoryEvent::ToolResult {
                    id,
                    output,
                    is_error,
                    details,
                    ..
                } => {
                    let Some((name, base)) = calls.get(id) else {
                        continue;
                    };
                    let mut content = base.clone();
                    if let Some(t) = mapping::end_text(name, output, *is_error) {
                        content.push(mapping::text_content(&t));
                    }
                    let mut update = json!({
                            "sessionUpdate": "tool_call_update", "toolCallId": id,
                            "status": if *is_error { "failed" } else { "completed" },
                            "content": content,
                            "rawOutput": {"output": clip(output, 16 * 1024), "isError": is_error, "details": details},
                        });
                    if !details.edits.is_empty() {
                        update["locations"] = json!(mapping::edit_locations(details));
                    }
                    out.update(sid, update);
                }
                HistoryEvent::ScriptCall { call, result, .. } => {
                    if let Some(result) = result {
                        out.update(sid, json!({"sessionUpdate":"tool_call_update", "toolCallId":call.id,
                            "status":if result.is_error {"failed"} else {"completed"},
                            "content":[mapping::text_content(&result.output)], "rawOutput":result,
                            "locations":mapping::edit_locations(&result.details)}));
                    } else {
                        out.update(sid, json!({"sessionUpdate":"tool_call", "toolCallId":call.id,
                            "title":mapping::tool_title(&call.name,&call.arguments,Some(&cwd)),
                            "kind":mapping::tool_kind(&call.name), "status":"in_progress", "rawInput":call.arguments}));
                    }
                }
                // The leading summary of a later context.
                HistoryEvent::Compaction { summary } if !spoke => out.update(
                    sid,
                    json!({"sessionUpdate": "agent_message_chunk", "content": text_block(&format!("\n\n_Context compacted._\n\n{}\n\n", clip(summary, 16 * 1024)))}),
                ),
                _ => {}
            }
        }
    }
    if let Some(Value::Array(items)) = todos {
        let entries: Vec<Value> = items
            .iter()
            .map(|i| {
                json!({
                    "content": i["content"].as_str().unwrap_or(""),
                    "priority": "medium",
                    "status": match i["status"].as_str() {
                        Some("completed") => "completed",
                        Some("in_progress") => "in_progress",
                        _ => "pending",
                    },
                })
            })
            .collect();
        out.update(sid, json!({"sessionUpdate": "plan", "entries": entries}));
    }
    let title = session.title();
    if !title.is_empty() {
        out.update(
            sid,
            json!({"sessionUpdate": "session_info_update", "title": title}),
        );
    }
}
