//! D137 advertisement is independent of D111 availability. Filter capabilities first.
use crate::{tool_names::Names, tools::ToolDefinition};
use code_config::{Snapshot, tools::Profile};
use std::collections::BTreeMap;

pub(crate) fn snapshots(snapshot: &Snapshot) -> BTreeMap<String, Profile> {
    let global = snapshot.config.tools.profile;
    let mut profiles = BTreeMap::from([(String::new(), global)]);
    for (pid, provider) in &snapshot.providers {
        let inherited = provider.tools.profile.unwrap_or(global);
        profiles.insert(format!("{pid}/"), inherited);
        for (mid, model) in &provider.models {
            profiles.insert(
                format!("{pid}/{mid}"),
                model.tools.profile.unwrap_or(inherited),
            );
        }
    }
    profiles
}
pub(crate) fn selected(profiles: &BTreeMap<String, Profile>, model: &str) -> Profile {
    profiles
        .get(model)
        .or_else(|| {
            model
                .split_once('/')
                .and_then(|(p, _)| profiles.get(&format!("{p}/")))
        })
        .or_else(|| profiles.get(""))
        .copied()
        .unwrap_or_default()
}
/// `catalog` already excludes disabled, unsupported and role-forbidden tools.
/// Extensions remain in that catalog; this function only selects native advertising.
pub(crate) fn native(
    catalog: &[ToolDefinition],
    profile: Profile,
    names: &Names,
) -> Vec<ToolDefinition> {
    catalog
        .iter()
        .filter(|tool| {
            !matches!(
                names.canonical(&tool.name),
                "graph" | "context_pack" | "change_impact"
            ) && (profile == Profile::Full
                || matches!(
                    names.canonical(&tool.name),
                    "read"
                        | "write"
                        | "edit"
                        | "bash"
                        | "grep"
                        | "glob"
                        | "code"
                        | "script"
                        | "history"
                        | "question"
                        | "terminal"
                        | "answer"
                        | "escalate"
                        | "message"
                        | "task"
                        | "agents"
                        | "jobs"
                        | "scope"
                        | "criteria"
                        | "skill"
                        | "report"
                        | "compact"
                ))
        })
        .cloned()
        .map(|mut tool| {
            if let Some((description, _)) = tool.description.split_once("\nDiscovery: ") {
                tool.description = description.to_owned();
            }
            tool
        })
        .collect()
}
pub(crate) const HINT: &str = "Extensions via script: graph for relationship shortlists, context_pack for scoped maps, change_impact for explicit diff impact, available lsp for semantic identity, web_search/web_fetch for external pages, models for unfamiliar child models, decide/decisions for lasting choices. Known tool: call directly or tools.describe(name=...) for its schema. tools.search(query=...) returns a list of dicts, tools.describe a dict with schema. grep(pattern=...) searches files. Inspect tools.describe(name='...'), then call the described function. code and history are native; batch/filter before printing bounded answers.";

/// Hints become part of the frozen catalog; D111 description overrides may replace them.
pub(crate) fn add_hints(definitions: &mut [ToolDefinition]) {
    for tool in definitions {
        let hint = match tool.name.as_str() {
            "code" => {
                "Need a definition or several outlines? Use native code symbol_find/outline; in script print only relevant names/ranges."
            }
            "graph" => {
                "Tracing several import/call edges? In script inspect tools.describe(name='graph'), then call graph(...); verify missing/ambiguous edges in source or available LSP."
            }
            "context_pack" => {
                "Need a bounded map of a known multi-file target? In script inspect tools.describe(name='context_pack'), then call context_pack(...); read relevant source afterward."
            }
            "change_impact" => {
                "Need downstream hints for a specified diff? In script inspect tools.describe(name='change_impact'), then call change_impact(base=...,target=...,path=...); use git diff for the patch."
            }
            "lsp" => {
                "Need semantic navigation with a detected server? In script inspect tools.describe(name='lsp'), then call lsp(...); fall back once if unavailable."
            }
            "web_search" => {
                "Need current external docs without a URL? In script inspect tools.describe(name='web_search'), then call web_search(...) and fetch a selected source; requires web.provider."
            }
            "web_fetch" => {
                "Have a documentation/page URL? In script inspect tools.describe(name='web_fetch'), then call web_fetch(url=...); follow returned offsets."
            }
            "models" => {
                "Choosing an unfamiliar child model or fixing eligibility? In script inspect tools.describe(name='models'), then models.list/describe/rank(...); reuse known valid IDs."
            }
            "decide" => {
                "Making a consequential tradeoff worth preserving? In script inspect tools.describe(name='decide'), then decide(...); skip trivial or automatically recorded choices."
            }
            "decisions" => {
                "Root needs an earlier choice or rationale? In script inspect tools.describe(name='decisions'), then decisions.search/list(...)."
            }
            "history" => {
                "Need an earlier exchange or child answer? Use native history with relevant agent/context and bounded fields; Monty history.query/search/read also works."
            }
            "read_image" => {
                "If image-capable and the answer depends on pixels, use native read_image; for a media extension inspect tools.describe(name='read_image') in script."
            }
            _ => continue,
        };
        tool.description.push_str("\nDiscovery: ");
        tool.description.push_str(hint);
    }
}
