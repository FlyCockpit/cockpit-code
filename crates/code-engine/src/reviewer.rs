//! A reviewer has its own immutable prompt/tools and append-only file library.
use crate::{
    Control,
    model::Model,
    read_only::{ReadOnly, Receipt},
    schema::*,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(crate) struct Libraries(BTreeMap<String, Library>);
struct Library {
    prompt: String,
    prompt_hash: String,
    guidance: Vec<(PathBuf, String)>,
    references: Vec<Receipt>,
}
fn guidance(cwd: &Path) -> Vec<(PathBuf, String)> {
    let mut paths = Vec::new();
    if let Some(root) = cwd.ancestors().find(|p| p.join(".git").exists()) {
        paths.push(root.join("AGENTS.md"));
    }
    let project = cwd.join("AGENTS.md");
    if !paths.contains(&project) {
        paths.push(project);
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok()?;
            if !meta.is_file() || meta.len() > 128 * 1024 {
                return None;
            }
            let resolver = crate::paths::Resolver::new(cwd.to_owned());
            let resolved = resolver.resolve(path.to_str()?).ok()?;
            let root = cwd
                .ancestors()
                .find(|p| p.join(".git").exists())
                .unwrap_or(cwd);
            if !resolved.starts_with(cwd) && !resolved.starts_with(root) {
                return None;
            }
            std::fs::read_to_string(&resolved)
                .ok()
                .map(|text| (resolved, text))
        })
        .collect()
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn check(
    libraries: &mut Libraries,
    control: &Control,
    model: &Model,
    cwd: &Path,
    prompt: &str,
    context: Value,
    agent: &str,
    safety_only: bool,
    cancel: &CancellationToken,
) -> Result<Value, EngineError> {
    let category = context["action"]["call"]["name"].as_str().unwrap_or("");
    let key = format!(
        "{}/{}/{}/{}",
        model.provider_id,
        model.model_id,
        cwd.display(),
        format_args!(
            "{}:{:x}",
            matches!(category, "write" | "edit"),
            Sha256::digest(prompt.as_bytes())
        )
    );
    let current_guidance = guidance(cwd);
    let prompt_hash = format!("{:x}", Sha256::digest(prompt.as_bytes()));
    let library = libraries.0.entry(key).or_insert_with(|| Library {
        prompt: prompt.into(),
        prompt_hash: prompt_hash.clone(),
        guidance: current_guidance.clone(),
        references: vec![],
    });
    if library.guidance != current_guidance || library.references.iter().any(|r| !r.unchanged()) {
        library.references.clear();
        library.guidance = current_guidance;
    }
    let mut history = vec![HistoryEvent::User {
        attachments: vec![],
        text: json!({"guidance":library.guidance}).to_string(),
    }];
    history.extend(
        library.references.iter().map(|r| HistoryEvent::User {
            attachments: vec![],
            text: json!({"reference":r.path,"sha256":r.hash,"read":r.args,"content":r.output})
                .to_string(),
        }),
    );
    if library.prompt_hash != prompt_hash {
        history.push(HistoryEvent::SystemReminder { text:crate::injection::notice("The configured reviewer prompt changed. This review retains its previously sent system prompt.") });
    }
    history.push(HistoryEvent::User {
        attachments: vec![],
        text: context.to_string(),
    });
    history.push(HistoryEvent::SystemReminder { text:crate::injection::notice(format!(
        "Review this prepared action. Read referenced guidance when needed. Read-only capabilities are available through Monty script: read(path=...), grep(pattern=...,path=...), glob(pattern=...,path=...). {} Return the final verdict as plain JSON {{\"verdict\":\"safe\"|\"unsafe\"{},\"reason\":string,\"hint\":string}}. {}",
        crate::read_only::ALLOWED, if safety_only { "" } else { "|\"improve\"" },
        if safety_only { "This is the final safety-only check. Never request another improvement round." } else { "Use improve only for a safe action for which a concrete better variation is warranted." },
    )) });
    let overrides = crate::tool_names::merged(&control.config.lock().unwrap(), &model.id());
    let names = crate::tool_names::names(&overrides);
    let definitions = crate::tool_names::apply(ReadOnly::definitions(), &overrides)?;
    let reader = ReadOnly::new(cwd.to_owned())
        .with_outputs(control.outputs_dir())
        .with_aliases(names.names.clone())
        .with_allowed(
            definitions
                .iter()
                .map(|d| names.canonical(&d.name).to_owned())
                .collect(),
        );
    let session = control.journal.lock().unwrap().header.id.clone();
    let mut request = model
        .prepare(
            &library.prompt,
            &definitions,
            &history,
            &format!("{session}:safety"),
        )?
        .cache_references()?;
    request.session_id = session;
    request.tool_aliases = names.names;
    request.agent_id = agent.into();
    request.kind = "safety".into();
    let result = async {
        loop {
            let completion = control.auxiliary(model, &request, cancel).await?;
            if !completion.status.executable() {
                return Err(EngineError::bad("reviewer completion unavailable"));
            }
            if completion.tool_calls.is_empty() {
                let text = completion
                    .text
                    .trim()
                    .trim_start_matches("```json")
                    .trim_start_matches("```")
                    .trim_end_matches("```")
                    .trim();
                let verdict: Value = serde_json::from_str(text)
                    .map_err(|_| EngineError::bad("reviewer returned invalid verdict JSON"))?;
                if !matches!(
                    verdict["verdict"].as_str(),
                    Some("safe" | "unsafe" | "improve")
                ) || (safety_only && verdict["verdict"] == "improve")
                {
                    return Err(EngineError::bad(
                        "reviewer returned invalid safety-only verdict",
                    ));
                }
                return Ok(verdict);
            }
            let calls = completion.tool_calls.clone();
            let mut tail = vec![HistoryEvent::Assistant {
                text: completion.text,
                reasoning: completion.reasoning,
                tool_calls: completion.tool_calls,
                replay: completion.replay,
                status: completion.status,
                native_reason: completion.stop_reason,
            }];
            for call in calls {
                let result = reader.call(&call, cancel).await;
                tail.push(HistoryEvent::ToolResult {
                    details: Default::default(),
                    id: call.id,
                    name: call.name,
                    is_error: result.is_err(),
                    output: result.unwrap_or_else(|e| e.message),
                });
            }
            request = request.with_history_tail(&tail)?;
        }
    }
    .await;
    for receipt in reader.receipts.lock().unwrap().iter() {
        if library
            .guidance
            .iter()
            .any(|(path, _)| path == &receipt.path)
        {
            continue;
        }
        if !library
            .references
            .iter()
            .any(|r| r.path == receipt.path && r.hash == receipt.hash && r.args == receipt.args)
        {
            library.references.push(receipt.clone());
        }
    }
    result
}
