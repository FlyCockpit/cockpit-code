//! Opt-in recovery accepts only a whole, tool-shaped JSON payload, never prose examples.
use crate::{schema::ToolCall, tools::ToolDefinition};
use serde_json::Value;
use uuid::Uuid;

pub fn rescue(text: &str, reasoning: &str, calls_empty: bool) -> Option<String> {
    (calls_empty && text.trim().is_empty() && !reasoning.trim().is_empty()).then(|| {
        format!(
            "*(Model put its answer in the reasoning channel — surfacing it here.)*\n\n{reasoning}"
        )
    })
}
pub fn recover(text: &str, tools: &[ToolDefinition]) -> Option<Vec<ToolCall>> {
    if tools.is_empty() {
        return None;
    }
    let mut payload = text.trim();
    if let Some(inner) = payload.strip_prefix("<tool_call>") {
        payload = inner.strip_suffix("</tool_call>")?.trim();
    } else if let Some(inner) = payload.strip_prefix("```") {
        let (language, body) = inner.split_once('\n')?;
        if !matches!(language.trim(), "" | "json" | "tool_call") {
            return None;
        }
        payload = body.strip_suffix("```")?.trim();
        if payload.contains("```") {
            return None;
        }
    }
    let value: Value = serde_json::from_str(payload).ok()?;
    let values = match value {
        Value::Array(values) => values,
        other => vec![other],
    };
    if values.is_empty() {
        return None;
    }
    values
        .into_iter()
        .map(|value| {
            let f = value.get("function").unwrap_or(&value);
            let name = f["name"].as_str()?;
            let args = f
                .get("arguments")
                .or_else(|| f.get("parameters"))
                .cloned()
                .unwrap_or_else(|| {
                    let mut flat = f.as_object().cloned().unwrap_or_default();
                    for k in ["name", "type", "id"] {
                        flat.remove(k);
                    }
                    Value::Object(flat)
                });
            // Unknown tools and invalid args deliberately reach the ordinary failed
            // result path, which nudges the model with the actual offered names.
            Some(ToolCall {
                id: format!("call_text_{}", Uuid::new_v4().simple()),
                name: name.into(),
                complete: true,
                arguments: args,
            })
        })
        .collect()
}

const MARKERS: &[&str] = &[
    "<|channel>",
    "<|channel|>",
    "<|im_start>",
    "<|im_start|>",
    "<|im_end>",
    "<|im_end|>",
    "<|start>",
    "<|start|>",
    "<|end>",
    "<|end|>",
    "<|message>",
    "<|message|>",
    "<|return>",
    "<|return|>",
    "<|assistant>",
    "<|assistant|>",
    "<|system>",
    "<|system|>",
    "<|user>",
    "<|user|>",
];
const HEADERS: &[&str] = &[
    "<|im_start|>assistant\n",
    "<|im_start>assistant\n",
    "<|start|>assistant<|channel|>analysis<|message|>",
    "<|start|>assistant<|channel|>final<|message|>",
    "<|start|>assistant<|message|>",
];
#[derive(Default)]
pub(crate) struct LeakStripper {
    pending: String,
    ready: bool,
    pub stripped: bool,
}
impl LeakStripper {
    pub fn push(&mut self, delta: &str, finish: bool) -> String {
        if self.ready {
            return delta.into();
        }
        self.pending.push_str(delta);
        let mut text = self.pending.as_str();
        loop {
            if !finish
                && (text.is_empty() || HEADERS.iter().chain(MARKERS).any(|m| m.starts_with(text)))
            {
                return String::new();
            }
            if let Some(header) = HEADERS
                .iter()
                .filter(|h| text.starts_with(**h))
                .max_by_key(|h| h.len())
            {
                self.stripped = true;
                text = text[header.len()..].trim_start();
                continue;
            }
            if let Some(marker) = MARKERS
                .iter()
                .filter(|m| text.starts_with(**m))
                .max_by_key(|m| m.len())
            {
                self.stripped = true;
                text = text[marker.len()..].trim_start();
                continue;
            }
            self.ready = true;
            return text.to_owned();
        }
    }
}
