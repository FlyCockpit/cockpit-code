//! Journal-derived facts used by compaction and partial child reports.
use crate::{Control, schema::*, sessions};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn facts(control: &Control, agent: &str) -> String {
    let names = control.tool_names(agent);
    let j = control.journal.lock().unwrap();
    let events: Vec<_> = sessions::read_contexts(&j.dir)
        .ok()
        .and_then(|mut all| all.remove(agent))
        .map(|contexts| contexts.into_values().flatten().map(|e| e.event).collect())
        .unwrap_or_else(|| j.histories.get(agent).cloned().unwrap_or_default());
    let mut calls = BTreeMap::new();
    let mut results = BTreeMap::new();
    let mut edits = BTreeSet::new();
    let mut last = String::from("none recorded");
    for event in &events {
        match event {
            HistoryEvent::ToolCall { call } | HistoryEvent::CallUpdate { call, .. } => {
                calls.insert(call.id.clone(), call.clone());
                last = format!("{} {}", call.name, call.arguments);
            }
            HistoryEvent::ToolResult {
                id,
                name,
                output,
                is_error,
                details,
            } => {
                for edit in &details.edits {
                    edits.insert(edit.path.display().to_string());
                }
                results.insert(id.clone(), (*is_error, output.clone()));
                last = format!("{name}: {}", if *is_error { "failed" } else { "completed" });
            }
            HistoryEvent::ScriptCall {
                call,
                result: Some(result),
                ..
            } => {
                calls.insert(call.id.clone(), call.clone());
                results.insert(call.id.clone(), (result.is_error, result.output.clone()));
            }
            _ => {}
        }
    }
    let mut reads = BTreeSet::new();
    let mut commands = vec![];
    let contract = j.contracts.get(agent).cloned().unwrap_or_default();
    let verification_command = regex::Regex::new(r"(?:^|[;&|]\s*)(?:cargo\s+(?:test|check|clippy|fmt|build)|(?:npm|pnpm|yarn|bun)\s+(?:run\s+)?(?:test|lint|build|typecheck)|go\s+test|pytest|python[0-9.]*\s+-m\s+(?:pytest|compileall|json.tool)|git\s+diff\s+--check|node\s+--check|tsc\b)").unwrap();
    for (id, call) in &calls {
        let Some((failed, _)) = results.get(id) else {
            continue;
        };
        let name = names.canonical(&call.name);
        if matches!(name, "read" | "read_image" | "read_video")
            && !failed
            && let Some(path) = call.arguments["path"].as_str()
        {
            reads.insert(path.to_owned());
        }
        if matches!(name, "write" | "edit")
            && !failed
            && let Some(path) = call.arguments["path"].as_str()
        {
            edits.insert(path.to_owned());
        }
        if name == "bash"
            && let Some(command) = call.arguments["command"].as_str()
        {
            let verification = contract
                .criteria()
                .filter_map(|c| c.check.as_ref())
                .any(|c| c.command == command)
                || verification_command.is_match(command);
            commands.push(format!(
                "- {}{}: {}",
                if *failed { "FAIL" } else { "PASS" },
                if verification { " [verification]" } else { "" },
                command
            ));
        }
    }
    let criteria = contract
        .criteria()
        .map(|c| {
            format!(
                "- {} [{}]: {} (evidence: {})",
                c.id, c.status, c.text, c.evidence
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let partial = j
        .partial_reports
        .get(agent)
        .map(|text| format!("Latest partial report:\n{text}\n\n"))
        .unwrap_or_default();
    format!(
        "{partial}Files read:\n{}\nFiles edited:\n{}\nCommands:\n{}\nLast action: {last}\nCriteria state:\n{criteria}",
        reads
            .into_iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n"),
        edits
            .into_iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n"),
        commands.join("\n")
    )
}
