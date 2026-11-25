//! Frozen display definitions route to canonical implementations at dispatch.
use crate::{
    Control, State,
    schema::{EngineError, HistoryEvent},
    tools::ToolDefinition,
};
use code_config::{ConfigStore, tools::ToolOverride};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
pub(crate) type Overrides = BTreeMap<String, ToolOverride>;
#[derive(Clone, Default)]
pub(crate) struct Names {
    pub names: BTreeMap<String, String>,
}
impl Names {
    pub fn configured<'a>(&'a self, canonical: &'a str) -> &'a str {
        self.names
            .get(canonical)
            .map(String::as_str)
            .unwrap_or(canonical)
    }
    pub fn canonical<'a>(&'a self, name: &'a str) -> &'a str {
        self.names
            .iter()
            .find_map(|(c, n)| (n == name).then_some(c.as_str()))
            .unwrap_or(name)
    }
    /// Engine-owned prose only. Never pass file/tool/user content here.
    pub fn rewrite(&self, text: &str) -> String {
        static IDENT: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"\b[A-Za-z_][A-Za-z_0-9]*\b").unwrap());
        // With no renames every identifier maps to itself.
        if self.names.is_empty() {
            return text.to_owned();
        }
        IDENT
            .replace_all(text, |c: &regex::Captures<'_>| {
                let span = c.get(0).unwrap();
                let prefix = &text[..span.start()];
                let suffix = text[span.end()..].trim_start();
                let mut quote = None;
                let mut previous = None;
                let mut escaped = false;
                for ch in prefix.chars() {
                    if escaped {
                        escaped = false;
                        previous = Some(ch);
                        continue;
                    }
                    if ch == '\\' {
                        escaped = true;
                        previous = Some(ch);
                        continue;
                    }
                    if let Some(q) = quote {
                        if ch == q {
                            quote = None;
                        }
                    } else if matches!(ch, '\'' | '"')
                        && !(ch == '\'' && previous.is_some_and(char::is_alphanumeric))
                    {
                        quote = Some(ch);
                    }
                    previous = Some(ch);
                }
                // Method/action names, argument keys and literal values are fixed schema.
                if quote.is_some() || prefix.ends_with(['/', '.']) || suffix.starts_with(['=', ':'])
                {
                    c[0].to_owned()
                } else {
                    self.configured(&c[0]).to_owned()
                }
            })
            .into_owned()
    }
}
pub(crate) fn snapshots(snapshot: &ConfigStore) -> BTreeMap<String, Overrides> {
    let mut out: BTreeMap<_, _> = snapshot
        .models()
        .map(|(p, _, m, _)| {
            let id = format!("{p}/{m}");
            let overrides = snapshot.tool_overrides(&id);
            (id, overrides)
        })
        .collect();
    out.insert(String::new(), snapshot.tool_overrides(""));
    for (provider, _) in snapshot.providers() {
        out.insert(
            format!("{provider}/"),
            snapshot.tool_overrides(&format!("{provider}/")),
        );
    }
    out
}
pub(crate) fn frozen(snapshots: &BTreeMap<String, Overrides>, id: &str) -> Overrides {
    snapshots
        .get(id)
        .or_else(|| {
            id.split_once('/')
                .and_then(|(p, _)| snapshots.get(&format!("{p}/")))
        })
        .or_else(|| snapshots.get(""))
        .cloned()
        .unwrap_or_default()
}
pub(crate) fn names(overrides: &Overrides) -> Names {
    Names {
        names: overrides
            .iter()
            .filter_map(|(c, o)| o.name.as_ref().map(|n| (c.clone(), n.clone())))
            .collect(),
    }
}
pub(crate) fn apply(
    mut defs: Vec<ToolDefinition>,
    overrides: &Overrides,
) -> Result<Vec<ToolDefinition>, EngineError> {
    let names = names(overrides);
    let canonical = crate::tool_defaults::definitions()
        .into_iter()
        .map(|d| d.name)
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    for (key, o) in overrides {
        if !canonical.contains(key) {
            return Err(EngineError::bad(format!("unknown tool override {key}")));
        }
        let name = o.name.as_deref().unwrap_or(key);
        if [
            "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
            "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
            "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise",
            "return", "try", "while", "with", "yield", "tools", "skills", "mcp", "store", "load",
        ]
        .contains(&name)
            || name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .enumerate()
                .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
            || (name != key && canonical.contains(name))
            || !seen.insert(name)
        {
            return Err(EngineError::bad(format!(
                "invalid or colliding tool name {name}"
            )));
        }
    }
    // Also catch collisions with tools having no explicit override.
    let mut offered = BTreeSet::new();
    for d in &mut defs {
        let canonical = d.name.clone();
        if let Some(o) = overrides.get(&canonical) {
            if let Some(properties) = d.schema["properties"].as_object_mut() {
                for (name, p) in &o.parameters {
                    let prop = properties.get_mut(name).ok_or_else(|| {
                        EngineError::bad(format!("unknown parameter {canonical}.{name}"))
                    })?;
                    if let Some(text) = &p.description {
                        prop["description"] = serde_json::json!(text);
                    }
                }
            }
            d.description = o
                .description
                .clone()
                .unwrap_or_else(|| names.rewrite(&d.description));
        } else {
            d.description = names.rewrite(&d.description);
        }
        d.name = names.configured(&canonical).to_owned();
        if !offered.insert(d.name.clone()) {
            return Err(EngineError::bad("tool name collision"));
        }
    }
    defs.retain(|d| {
        overrides
            .get(names.canonical(&d.name))
            .is_none_or(|o| o.enabled != Some(false))
    });
    Ok(defs)
}
impl State {
    pub(crate) fn tool_overrides(&self) -> Overrides {
        let id = self.model().map(|m| m.id()).unwrap_or_default();
        self.context_state
            .frozen
            .as_ref()
            .map(|c| c.overrides(&id))
            .unwrap_or_else(|| self.store.tool_overrides(&id))
    }
    pub(crate) fn tool_names(&self) -> Names {
        names(&self.tool_overrides())
    }
}
impl Control {
    pub(crate) fn tool_names(&self, agent: &str) -> Names {
        let config = self.config.lock().unwrap().clone();
        let j = self.journal.lock().unwrap();
        let id = if agent == "root" {
            j.settings
                .model
                .as_deref()
                .or(config.config().default_model.as_deref())
        } else {
            j.agents.get(agent).and_then(|a| a.model.as_deref())
        }
        .unwrap_or("");
        let actual = j
            .requests
            .get(agent)
            .filter(|_| j.activities.contains_key(agent))
            .and_then(|p| {
                let body: serde_json::Value = serde_json::from_slice(&p.request.body).ok()?;
                Some(format!("{}/{}", p.provider_id, body["model"].as_str()?))
            });
        let id = actual.as_deref().unwrap_or(id);
        let context = j.histories.get(agent).and_then(|h| {
            h.iter().find_map(|e| {
                if let HistoryEvent::PromptContext { context } = e {
                    Some(context)
                } else {
                    None
                }
            })
        });
        if let Some(context) = context {
            return names(&context.overrides(id));
        }
        names(&config.tool_overrides(id))
    }
}
pub fn warnings(snapshot: &ConfigStore, id: &str) -> Vec<String> {
    let overrides = snapshot.tool_overrides(id);
    let mut out = vec![];
    for core in ["read", "edit", "write", "bash"] {
        if overrides
            .get(core)
            .is_some_and(|o| o.enabled == Some(false))
        {
            out.push(format!(
                "{id}: core tool {} is disabled",
                names(&overrides).configured(core)
            ));
        }
    }
    if let Err(error) = apply(crate::tool_defaults::definitions(), &overrides) {
        out.push(format!("{id}: {}", error.message));
    }
    out
}

/// Invalid emitted names must never poison the next provider request. Keep the
/// canonical journal unchanged so the failed result can explain the real name.
pub(crate) fn sanitize_history(
    history: &[crate::schema::HistoryEvent],
) -> Vec<crate::schema::HistoryEvent> {
    use crate::schema::HistoryEvent;
    fn safe(name: &str) -> String {
        if !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return name.into();
        }
        format!(
            "invalid_tool_{}",
            &crate::context::digest(name.as_bytes())[..16]
        )
    }
    let mut out = history.to_vec();
    for event in &mut out {
        match event {
            HistoryEvent::Assistant {
                tool_calls, replay, ..
            } => {
                for call in tool_calls {
                    call.name = safe(&call.name);
                }
                if let Some(replay) = replay {
                    for item in &mut replay.items {
                        if matches!(item["type"].as_str(), Some("tool_use" | "function_call"))
                            && let Some(name) = item["name"].as_str()
                        {
                            item["name"] = serde_json::json!(safe(name));
                        }
                    }
                }
            }
            HistoryEvent::ToolCall { call }
            | HistoryEvent::CallUpdate { call, .. }
            | HistoryEvent::ScriptCall { call, .. } => call.name = safe(&call.name),
            HistoryEvent::ToolResult { name, .. } => *name = safe(name),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_search_override_is_an_unknown_tool() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("tools.json"), r#"{"search":{}}"#).unwrap();
        let store = ConfigStore::load(code_config::Directories {
            config: temp.path().into(),
            data: temp.path().join("data"),
            cache: temp.path().join("cache"),
        })
        .unwrap();
        let overrides = store.tool_overrides("");
        assert!(overrides.contains_key("search"));
        let error = apply(crate::tool_defaults::definitions(), &overrides).unwrap_err();
        assert_eq!(error.message, "unknown tool override search");
    }

    #[test]
    fn current_intel_overrides_rename_and_disable_tools() {
        let overrides: Overrides = serde_json::from_value(serde_json::json!({
            "code":{"name":"syntax"}, "graph":{"enabled":false}
        }))
        .unwrap();
        let defs = apply(crate::intel::definitions(), &overrides).unwrap();
        assert!(defs.iter().any(|d| d.name == "syntax"));
        assert!(
            !defs
                .iter()
                .any(|d| d.name == "code" || d.name == "graph" || d.name == "search")
        );
        assert_eq!(names(&overrides).canonical("syntax"), "code");
    }
}
