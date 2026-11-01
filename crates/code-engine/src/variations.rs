//! Best-of-N candidates share the author's exact prepared request (D56).
use crate::{
    Control,
    hidden_fork::{ForkTask, HiddenFork},
    model::Model,
    read_only::ReadOnly,
    schema::*,
    tools::PreparedOperation,
};
use code_config::Snapshot;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

impl Control {
    pub(crate) async fn private_fork(
        &self,
        fork: &HiddenFork,
        task: &ForkTask,
        cancel: &CancellationToken,
    ) -> Result<(), EngineError> {
        self.private_fork_for(
            fork,
            task,
            &crate::scopes::execution_owner(&fork.agent_id),
            cancel,
        )
        .await
    }
    pub(crate) async fn private_fork_for(
        &self,
        fork: &HiddenFork,
        task: &ForkTask,
        scheduling_owner: &str,
        cancel: &CancellationToken,
    ) -> Result<(), EngineError> {
        crate::exclusive::check_model(&self.journal.lock().unwrap().settings, &fork.model)?;
        let display = if fork.purpose == "guidance" {
            format!("guidance:{}", fork.agent_id)
        } else if scheduling_owner != fork.agent_id {
            format!("scope-aux:{}", fork.agent_id)
        } else {
            fork.agent_id.clone()
        };
        let _slot = self
            .scheduler
            .short_named(scheduling_owner, &display, &fork.model, cancel)
            .await?;
        self.check_owner()?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = {
            let pending = fork.run_for(&tx, cancel, task);
            tokio::pin!(pending);
            loop {
                tokio::select! {
                    result = &mut pending => break result,
                    Some(EngineEvent::Usage { usage }) = rx.recv() => {
                        self.commit(&fork.agent_id, HistoryEvent::Usage { usage:usage.clone() })?;
                        self.emit(EngineEvent::Usage { usage });
                    }
                }
            }
        };
        drop(tx);
        while let Some(EngineEvent::Usage { usage }) = rx.recv().await {
            self.commit(
                &fork.agent_id,
                HistoryEvent::Usage {
                    usage: usage.clone(),
                },
            )?;
            self.emit(EngineEvent::Usage { usage });
        }
        for outcome in result? {
            self.commit(
                &fork.agent_id,
                HistoryEvent::HiddenForkOutcome {
                    outcome: outcome.clone(),
                },
            )?;
            self.emit(EngineEvent::HiddenForkFinished { outcome });
        }
        Ok(())
    }
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn select(
    control: &Control,
    snapshot: &Snapshot,
    op: &PreparedOperation,
    count: usize,
    hint: &str,
    safety_focuses: &[String],
    purpose: &str,
    cancel: &CancellationToken,
) -> Result<Option<(ToolCall, String)>, EngineError> {
    if count <= 1 {
        return Ok(None);
    }
    let checkpoint = control
        .journal
        .lock()
        .unwrap()
        .requests
        .get(&op.agent_id)
        .cloned()
        .ok_or_else(|| EngineError::bad("author checkpoint unavailable"))?;
    let provider_id = if checkpoint.provider_id.is_empty() {
        control
            .journal
            .lock()
            .unwrap()
            .agents
            .get(&op.agent_id)
            .and_then(|a| a.model.as_deref())
            .and_then(|id| id.split_once('/'))
            .map(|(p, _)| p.to_owned())
            .ok_or_else(|| EngineError::bad("author provider unavailable"))?
    } else {
        checkpoint.provider_id.clone()
    };
    let provider = snapshot
        .providers
        .get(&provider_id)
        .cloned()
        .ok_or_else(|| EngineError::bad("author provider unavailable"))?;
    let body: Value = serde_json::from_slice(&checkpoint.request.body)
        .map_err(|_| EngineError::bad("invalid author checkpoint"))?;
    let model_id = body["model"]
        .as_str()
        .ok_or_else(|| EngineError::bad("author model missing"))?;
    if checkpoint.config.kind.unwrap_or(provider.kind) == code_config::ProviderType::Command
        && provider
            .command
            .as_ref()
            .is_some_and(|c| c.history.as_deref() == Some("latest"))
    {
        return Ok(None);
    }
    let mut config = checkpoint.config.clone();
    config.reasoning.default = checkpoint.effort.clone();
    let model = Model::new(provider_id, model_id.into(), provider, config, None)?;
    let proposals = Arc::new(Mutex::new(BTreeMap::new()));
    let indirect = !checkpoint
        .request
        .offered_tools
        .iter()
        .any(|t| checkpoint.request.canonical_tool_name(&t.name) == op.call.name);
    let mut functions = vec![];
    if indirect {
        let proposals = proposals.clone();
        let original = op.call.clone();
        functions.push(crate::hidden_fork::ForkFunction {
            name: "propose_action".into(),
            description: "Propose arguments for the current action without executing it.".into(),
            call: Arc::new(move |index, args| {
                let arguments = args
                    .get("arguments")
                    .filter(|v| v.is_object())
                    .ok_or("propose_action requires an arguments object")?
                    .clone();
                if original.name == "mcp"
                    && (arguments["server"] != original.arguments["server"]
                        || arguments["tool"] != original.arguments["tool"])
                {
                    return Err("MCP variations must keep the same server and tool".into());
                }
                let mut call = original.clone();
                call.arguments = arguments;
                proposals.lock().unwrap().insert(index, call);
                Ok(serde_json::json!({"proposed":true}))
            }),
        });
    }
    let focuses = safety_focuses
        .iter()
        .chain(model.config.variation_focuses.iter())
        .cloned()
        .chain(std::iter::repeat("write an alternative".into()))
        .take(count - 1)
        .collect();
    let names = control.tool_names(&op.agent_id);
    let mut display_call = op.call.clone();
    display_call.name = names.configured(&display_call.name).into();
    let mut fork = HiddenFork {
        model,
        author: checkpoint.request,
        tail: crate::injection::notice(format!(
            "Proposed next action: {}. Reply with the single {} tool call you think is best. Repeat it unchanged if you agree. Change only its arguments. Hint: {}. {} Other actions are refused. Do not execute the proposed action; your returned call is a candidate. {}",
            serde_json::to_string(&display_call).unwrap(),
            display_call.name,
            hint,
            names.rewrite("Read-only capabilities are available through Monty: read, grep, glob."),
            if indirect {
                "This action is available through Monty only. Submit its arguments with script code propose_action(arguments={...}); that fork-only capability proposes without executing."
            } else {
                ""
            },
        )),
        copies: count - 1,
        max_steps: Some(6),
        functions,
        agent_id: op.agent_id.clone(),
        purpose: purpose.into(),
    };
    control
        .private_fork(
            &fork,
            &ForkTask::Candidate {
                focuses,
                name: op.call.name.clone(),
                reader: Box::new(ReadOnly::new(op.cwd.clone()).with_outputs(control.outputs_dir())),
                proposals: proposals.clone(),
            },
            cancel,
        )
        .await?;
    let mut candidates = vec![op.call.clone()];
    candidates.extend(proposals.lock().unwrap().values().cloned());
    if candidates.len() < 2 {
        return Ok(None);
    }
    let evaluated = candidates.len();
    let labelled = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut display = c.clone();
            display.name = names.configured(&display.name).into();
            format!("{}: {}", i, serde_json::to_string(&display).unwrap())
        })
        .collect::<Vec<_>>()
        .join("\n");
    fork.copies = 1;
    fork.max_steps = Some(3);
    fork.functions.clear();
    fork.tail = crate::injection::notice(format!(
        "Judge these proposed next actions. Original is candidate 0. Candidates:\n{labelled}\nReturn only JSON: {{\"action\":\"select\",\"index\":0,\"rationale\":\"why\"}} or {{\"action\":\"synthesize\",\"args\":{{...}},\"rationale\":\"why combine\",\"sources\":[1,2],\"fallbackIndex\":0}}. index/fallbackIndex use zero-based candidate numbers. A synthesis must retain the same tool and target and include your best single fallbackIndex. Do not call tools. Hint: {hint}",
    ));
    let selection = Arc::new(Mutex::new(None));
    control
        .private_fork(
            &fork,
            &ForkTask::Evaluator {
                selection: selection.clone(),
            },
            cancel,
        )
        .await?;
    let selected = selection
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| EngineError::bad("evaluator did not select"))?;
    let decision = selected
        .get("synthesize")
        .or_else(|| selected.get("select"))
        .unwrap_or(&selected);
    let synthesize = selected.get("synthesize").is_some() || selected["action"] == "synthesize";
    let legacy = selected.get("choice").is_some();
    let choice = if synthesize {
        decision["fallbackIndex"].as_u64()
    } else if legacy {
        selected["choice"].as_u64().and_then(|n| n.checked_sub(1))
    } else {
        decision["index"].as_u64()
    };
    let choice = choice
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n < candidates.len())
        .unwrap_or(0);
    let rationale = decision["rationale"]
        .as_str()
        .or_else(|| decision["reason"].as_str())
        .unwrap_or("selected by judge");
    let mut call = candidates[choice].clone();
    call.id = op.call.id.clone();
    let mut combined = false;
    if synthesize {
        let mut proposed = op.call.clone();
        proposed.arguments = decision["args"].clone();
        if validate_candidate(control, op, &fork, &mut proposed, cancel)
            .await
            .is_ok()
        {
            call = proposed;
            combined = true;
        }
    }
    validate_candidate(control, op, &fork, &mut call, cancel).await?;
    let note = if combined {
        format!(
            "synthesized from candidates {}: {rationale}",
            decision.get("sources").unwrap_or(&serde_json::json!([]))
        )
    } else {
        format!(
            "{} {evaluated} candidates; applied variation {choice}: {rationale}{}",
            if purpose == "self_eval" {
                "self-evaluated"
            } else {
                "improved"
            },
            if synthesize {
                "; synthesis invalid; used best single pick"
            } else {
                ""
            }
        )
    };
    Ok(Some((call, note)))
}
async fn validate_candidate(
    control: &Control,
    op: &PreparedOperation,
    fork: &HiddenFork,
    call: &mut ToolCall,
    cancel: &CancellationToken,
) -> Result<(), EngineError> {
    if call.name == "mcp" {
        if call.arguments["server"] != op.call.arguments["server"]
            || call.arguments["tool"] != op.call.arguments["tool"]
        {
            return Err(EngineError::bad("MCP synthesis must retain server/tool"));
        }
        let tools = control
            .mcp
            .catalog(call.arguments["server"].as_str().unwrap_or(""), cancel)
            .await
            .map_err(|_| EngineError::bad("MCP schema unavailable"))?;
        let tool = tools
            .iter()
            .find(|t| t.name == call.arguments["tool"].as_str().unwrap_or(""))
            .ok_or_else(|| EngineError::bad("MCP tool unavailable"))?;
        crate::repair::validate(
            &call.arguments["arguments"],
            &tool.input_schema,
            "arguments",
        )
        .map_err(EngineError::bad)?;
    } else {
        let mut definitions = fork.author.offered_tools.clone();
        for definition in &mut definitions {
            definition.name = fork.author.canonical_tool_name(&definition.name).to_owned();
        }
        if !definitions.iter().any(|d| d.name == call.name) {
            definitions.extend(crate::tools::ToolBox::definitions());
            definitions.extend(crate::session_tools::definitions());
            definitions.extend(crate::tree::definitions(None, true));
            definitions.extend(crate::tools::web::definitions());
        }
        crate::repair::repair(call, &definitions).map_err(EngineError::bad)?;
    }
    crate::tools::ToolBox::validate_combined_edit(op, call)
        .map_err(|e| EngineError::bad(e.to_string()))
}
