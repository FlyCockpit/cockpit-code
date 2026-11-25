//! Child launch: starting, continuing and running child agents within capacity.
use super::*;

pub(super) async fn start_child(
    state: &State,
    args: &Value,
    task_call_id: &str,
    cancel: &CancellationToken,
) -> Result<String, EngineError> {
    if state.control.flat_mode() {
        if state.agent_id != "root" {
            return Err(EngineError::bad(
                "flat-mode children cannot start subagents",
            ));
        }
        if args.get("maxDescendants").is_some() {
            return Err(EngineError::bad(
                "maxDescendants is unavailable in flat mode",
            ));
        }
    }
    let current_remaining = state.current_remaining_depth();
    if current_remaining == Some(0) {
        let j = state.control.journal.lock().unwrap();
        let depth = j.depth(&state.agent_id);
        let max = state
            .config
            .lock()
            .unwrap()
            .config()
            .subagents
            .max_depth
            .unwrap_or(depth);
        return Err(EngineError::bad(format!(
            "subagent depth exhausted: depth {depth} of {max}; cannot launch depth {}",
            depth + 1
        )));
    }
    let child_remaining = current_remaining.map(|depth| depth.saturating_sub(1));
    if args["continue"].as_str().is_some() {
        return continue_child(state, args, task_call_id, cancel, child_remaining).await;
    }
    let role = field(args, "agent")?;
    let stable_prompt = match role {
        "build" => BUILD_PROMPT,
        "plan" => PLAN_PROMPT,
        "explore" => EXPLORE_PROMPT,
        _ => return Err(EngineError::bad("agent must be build, plan or explore")),
    };
    let settings = state.model_settings();
    let inherited_model = args.get("model").is_none();
    if let Some(ex) = &settings.exclusive
        && args["model"].as_str().is_some_and(|id| id != ex.model)
    {
        return Err(EngineError::bad("task model is locked by exclusive mode"));
    }
    let model_id = match args.get("model") {
        Some(Value::String(id)) => id.clone(),
        None => settings
            .exclusive
            .as_ref()
            .map(|ex| ex.model.clone())
            .or(settings.model.clone())
            .as_ref()
            .cloned()
            .or_else(|| state.store.config().default_model.clone())
            .ok_or_else(|| {
                EngineError::bad(
                    "parent has no current model; pass an explicit subagent-eligible model",
                )
            })?,
        _ => return Err(EngineError::bad("model must be a provider/model string")),
    };
    let prompt = field(args, "prompt")?.to_owned();
    let missing_title = args.get("title").is_none();
    let title = task_title(args, &prompt)?;
    let (mut policy, inherited_budget) = {
        let j = state.control.journal.lock().unwrap();
        (
            j.executions
                .get(&state.agent_id)
                .map(|e| e.model_policy.clone())
                .unwrap_or_default(),
            crate::subtree::remaining(&j, &state.agent_id).map(|n| n.saturating_sub(1)),
        )
    };
    if let Some(value) = args.get("models") {
        let layer: crate::ModelPolicy =
            serde_json::from_value(value.clone()).map_err(|e| EngineError::bad(e.to_string()))?;
        layer.validate()?;
        policy.push(layer);
    }
    if !crate::subtree::permits(&policy, &model_id) {
        return Err(EngineError::bad(format!(
            "model {model_id} is disallowed by subtree policy; allowed models: {}",
            allowed_models(state, &policy).join(", ")
        )));
    }
    let max_descendants = match args.get("maxDescendants") {
        Some(value) => {
            let cap = value
                .as_u64()
                .ok_or_else(|| EngineError::bad("maxDescendants must be a nonnegative integer"))?;
            Some(inherited_budget.map_or(cap, |n| n.min(cap)))
        }
        None => inherited_budget,
    };
    let context = args["context"].as_str().unwrap_or("fresh");
    if !matches!(context, "fresh" | "fork") {
        return Err(EngineError::bad("context must be fresh or fork"));
    }
    let background = args["background"] == true;
    state.control.await_model(&model_id, cancel).await?;
    let ready_store = code_config::ConfigStore::load(state.store.directories())
        .map_err(|e| EngineError::bad(e.to_string()))?;
    let snapshot = &ready_store;
    let toggles = ModelToggles {
        disabled_models: settings.disabled_models.clone(),
        disabled_providers: settings.disabled_providers.clone(),
    };
    let (provider_id, model_name) = model_id
        .split_once('/')
        .ok_or_else(|| EngineError::bad("model must be provider/model"))?;
    let eligible = snapshot.model(&model_id).is_some_and(|(p, m)| {
        code_config::subagent_eligible(provider_id, model_name, p, m, &toggles)
    });
    if !eligible {
        return Err(EngineError::bad(if inherited_model {
            "parent's current model is not subagent-eligible; pass an explicit eligible model"
        } else {
            "model is not subagent-eligible (check capabilities, allowInSubagents and session toggles)"
        }));
    }
    let effort = match args.get("reasoningEffort") {
        Some(Value::String(effort)) => Some(effort.clone()),
        None => state.model()?.effort.clone(),
        _ => return Err(EngineError::bad("reasoningEffort must be a string")),
    };
    build_model(
        snapshot,
        &SessionSettings {
            model: Some(model_id.clone()),
            effort: effort.clone(),
            ..settings
        },
    )?;
    let resolver = paths::Resolver::new(state.cwd.clone());
    let cwd = resolver
        .resolve(args["cwd"].as_str().unwrap_or("."))
        .map_err(|e| EngineError::bad(e.to_string()))?;
    if !cwd.is_dir() {
        return Err(EngineError::bad("child cwd is not a directory"));
    }
    resolver
        .validate(&cwd)
        .map_err(|e| EngineError::bad(e.to_string()))?;
    let store = code_config::ConfigStore::load(state.store.directories())
        .map_err(|e| EngineError::bad(e.to_string()))?;
    let inherited = if context == "fork" {
        state.history()
    } else {
        vec![]
    };
    let remaining = child_remaining;
    let id = format!("{role}-{}", Uuid::new_v4());
    let agent = AgentRecord {
        agent_id: id.clone(),
        parent_id: Some(state.agent_id.clone()),
        agent: role.into(),
        model: Some(model_id.clone()),
        status: "running".into(),
        created_at: sessions::now(),
        stable_prompt: stable_prompt.into(),
    };
    let execution = AgentExecution {
        title,
        spawning_context: state
            .control
            .journal
            .lock()
            .unwrap()
            .contexts
            .get(&state.agent_id)
            .copied(),
        max_descendants,
        model_policy: policy,
        launch_id: Uuid::new_v4().to_string(),
        original_task: prompt.clone(),
        agent_id: id.clone(),
        cwd: cwd.clone(),
        remaining_depth: remaining,
        task: prompt.clone(),
        background,
        reasoning_effort: effort.clone(),
    };
    // Reserve under one journal lock, so simultaneous siblings cannot exceed either cap.
    {
        let mut j = state.control.journal.lock().unwrap();
        check_capacity(&j, &state.agent_id, snapshot)?;
        let scope = crate::scopes::task_scope(&j, &state.agent_id, &id, &cwd, args)?;
        j.register(agent.clone())?;
        let envelope = j.append(
            &id,
            HistoryEvent::AgentStarted {
                agent: agent.clone(),
                execution: execution.clone(),
                task_call_id: Some(task_call_id.into()),
            },
        )?;
        state.control.emit(EngineEvent::Committed { envelope });
        let envelope = j.append(&id, HistoryEvent::AgentScope { scope })?;
        state.control.emit(EngineEvent::Committed { envelope });
    }
    let mut reservation = AgentReservation {
        control: &state.control,
        id: id.clone(),
        armed: true,
    };
    state.control.emit(EngineEvent::AgentStarted {
        agent: agent.clone(),
        execution: execution.clone(),
        breadcrumb: state.control.breadcrumb(&id),
        task_call_id: Some(task_call_id.into()),
    });
    if !inherited.is_empty() {
        // Different models must replay the canonical view rather than another provider's native items.
        let mut history = inherited;
        history.retain(|event| {
            !matches!(
                event,
                HistoryEvent::PromptContext { .. }
                    | HistoryEvent::Goal { .. }
                    | HistoryEvent::GoalState { .. }
            )
        });
        for event in &mut history {
            if let HistoryEvent::Assistant { replay, .. } = event {
                *replay = None;
            }
        }
        state.control.commit(
            &id,
            HistoryEvent::HistoryReplace {
                history,
                reason: "fork parent context for child".into(),
            },
        )?;
    }
    let mut contract = crate::goal::GoalContract {
        objective: prompt.chars().take(4096).collect(),
        ..crate::goal::GoalContract::default()
    };
    contract.apply_task(args)?;
    state.control.save_contract(&id, contract)?;
    if let Some(paths) = args.get("subscribe") {
        let paths: Vec<PathBuf> =
            serde_json::from_value(paths.clone()).map_err(|e| EngineError::bad(e.to_string()))?;
        crate::subscriptions::task_subscriptions(state, &id, &paths, cancel).await?;
    }
    reservation.armed = false;
    let result = launch_child(
        state,
        ChildLaunch {
            prompt,
            agent,
            execution,
            checkpoint: None,
            store,
            idle_seconds: None,
            context_decision: None,
            steering: false,
        },
        cancel,
    )
    .await?;
    if missing_title {
        let mut value: Value =
            serde_json::from_str(&result).map_err(|e| EngineError::bad(e.to_string()))?;
        value["warning"] = json!(
            "task.title was missing; filled from the brief's first line (at most 80 characters)"
        );
        return Ok(value.to_string());
    }
    Ok(result)
}
fn task_title(args: &Value, prompt: &str) -> Result<String, EngineError> {
    match args.get("title") {
        Some(Value::String(title)) if !title.trim().is_empty() && title.chars().count() <= 80 => {
            Ok(title.trim().into())
        }
        Some(_) => Err(EngineError::bad("task.title must contain 1–80 characters")),
        None => Ok(prompt
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("Untitled task")
            .trim()
            .chars()
            .take(80)
            .collect()),
    }
}
fn allowed_models(state: &State, policy: &[crate::ModelPolicy]) -> Vec<String> {
    let snapshot = &state.store;
    let settings = state.model_settings();
    let toggles = ModelToggles {
        disabled_models: settings.disabled_models,
        disabled_providers: settings.disabled_providers,
    };
    snapshot
        .models()
        .filter_map(|(provider, p, model, m)| {
            let id = format!("{provider}/{model}");
            (code_config::subagent_eligible(provider, model, p, m, &toggles)
                && crate::subtree::permits(policy, &id))
            .then_some(id)
        })
        .collect()
}
fn check_capacity(j: &Journal, parent: &str, snapshot: &ConfigStore) -> Result<(), EngineError> {
    if j.finalizing
        .iter()
        .any(|ancestor| ancestor == parent || crate::agents::descendant(j, ancestor, parent))
    {
        return Err(EngineError::bad(
            "this subtree is finalizing; new launches and continuations are disabled",
        ));
    }
    if crate::subtree::remaining(j, parent) == Some(0) {
        return Err(EngineError::bad(
            "maxDescendants exhausted for this subtree; wait does not replenish the launch budget",
        ));
    }
    let alive = |a: &&AgentRecord| {
        (matches!(a.status.as_str(), "running" | "waiting" | "compacting")
            || a.status.starts_with("queued"))
    };
    let children = j
        .agents
        .values()
        .filter(|a| a.parent_id.as_deref() == Some(parent))
        .filter(alive)
        .count();
    if children >= snapshot.config().subagents.max_concurrent_children {
        return Err(EngineError::bad(
            "maxConcurrentChildren reached; wait for an existing child",
        ));
    }
    let total = j.agents.values().filter(alive).count();
    if snapshot
        .config()
        .subagents
        .max_total_agents
        .is_some_and(|n| total >= n)
    {
        return Err(EngineError::bad("maxTotalAgents reached"));
    }
    Ok(())
}
pub(super) async fn continue_child(
    state: &State,
    args: &Value,
    task_call_id: &str,
    cancel: &CancellationToken,
    remaining_depth: Option<usize>,
) -> Result<String, EngineError> {
    for key in ["agent", "model", "reasoningEffort", "cwd"] {
        if !args[key].is_null() {
            return Err(EngineError::bad(
                "continue accepts continue, prompt, background, criteria amendments, review and override",
            ));
        }
    }
    let requested_context = args["context"].as_str().unwrap_or("auto");
    if !matches!(requested_context, "auto" | "resume" | "fresh") {
        return Err(EngineError::bad(
            "continue context must be auto, resume or fresh",
        ));
    }
    let id = field(args, "continue")?;
    let prompt = field(args, "prompt")?.to_owned();
    let store = code_config::ConfigStore::load(state.store.directories())
        .map_err(|e| EngineError::bad(e.to_string()))?;
    let mut contract = state.control.contract(id);
    let before_resolution = contract.clone();
    contract.apply_task(args)?;
    for r in contract
        .roadblocks
        .iter_mut()
        .filter(|r| r.resolution.is_none())
    {
        let note = args["override"].as_str().unwrap_or(&prompt);
        if note.trim().is_empty() {
            return Err(EngineError::bad("roadblock pivot/override requires a note"));
        }
        r.resolution = Some(
            if args["override"].is_string() {
                "override"
            } else {
                "pivot"
            }
            .into(),
        );
        r.note = Some(note.into());
        r.updated = sessions::now();
        if args["override"].is_string()
            && let Some(id) = &r.criterion_id
        {
            for c in contract
                .acceptance
                .iter_mut()
                .chain(&mut contract.findings)
                .chain(&mut contract.rejection)
                .filter(|c| &c.id == id)
            {
                c.override_note = Some(note.into());
                c.status = if c.status == "hit" {
                    "withdrawn"
                } else {
                    "met"
                }
                .into();
                c.evidence = note.into();
                c.verification = "self-reported".into();
                c.updated = sessions::now();
            }
        }
    }
    contract.validate()?;
    let (agent, execution, checkpoint, idle) = {
        let mut j = state.control.journal.lock().unwrap();
        let mut agent = j
            .agents
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::bad("child not found"))?;
        if agent.parent_id.as_deref() != Some(&state.agent_id) {
            return Err(EngineError::bad(
                "only the creating parent may continue a child",
            ));
        }
        if !matches!(agent.status.as_str(), "done" | "failed" | "roadblocked") {
            return Err(EngineError::bad(
                "only done, failed or roadblocked children can continue; use message to steer a running child; killed children cannot continue",
            ));
        }
        check_capacity(&j, &state.agent_id, &state.store)?;
        // Status can become terminal just before the runner publishes its result.
        if state
            .control
            .runtime
            .agents
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|a| a.result.lock().unwrap().is_none())
        {
            return Err(EngineError::bad(
                "child is still finishing; retry at the next boundary",
            ));
        }
        let mut execution = j
            .executions
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::bad("child execution metadata unavailable"))?;
        execution.launch_id = Uuid::new_v4().to_string();
        execution.spawning_context = j.contexts.get(&state.agent_id).copied();
        if args.get("title").is_some() {
            execution.title = task_title(args, &prompt)?;
        }
        if args.get("models").is_some() || args.get("maxDescendants").is_some() {
            return Err(EngineError::bad(
                "continuations retain their subtree policy and budget; set them on the original launch",
            ));
        }
        execution.remaining_depth = remaining_depth;
        execution.task = prompt.clone();
        execution.background = args["background"] == true;
        let idle = j
            .agent_updated
            .get(id)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|ts| {
                (chrono::Utc::now() - ts.with_timezone(&chrono::Utc))
                    .num_milliseconds()
                    .max(0) as f64
                    / 1000.0
            })
            .unwrap_or(0.0);
        let scope = crate::scopes::task_scope(&j, &state.agent_id, id, &execution.cwd, args)?;
        agent.status = "running".into();
        j.update_agent(agent.clone())?;
        let envelope = j.append(
            id,
            HistoryEvent::AgentStarted {
                agent: agent.clone(),
                execution: execution.clone(),
                task_call_id: Some(task_call_id.into()),
            },
        )?;
        state.control.emit(EngineEvent::Committed { envelope });
        let envelope = j.append(id, HistoryEvent::AgentScope { scope })?;
        state.control.emit(EngineEvent::Committed { envelope });
        (agent, execution, j.requests.get(id).cloned(), idle)
    };
    let mut reservation = AgentReservation {
        control: &state.control,
        id: id.into(),
        armed: true,
    };
    state.control.emit(EngineEvent::AgentStarted {
        agent: agent.clone(),
        execution: execution.clone(),
        breadcrumb: state.control.breadcrumb(id),
        task_call_id: Some(task_call_id.into()),
    });
    state.control.save_contract(id, contract.clone())?;
    state.control.record_contract_resolutions(
        &state.agent_id,
        id,
        &before_resolution,
        &contract,
    )?;
    if let Some(paths) = args.get("subscribe") {
        let paths: Vec<PathBuf> =
            serde_json::from_value(paths.clone()).map_err(|e| EngineError::bad(e.to_string()))?;
        crate::subscriptions::task_subscriptions(state, id, &paths, cancel).await?;
    }
    let profile = checkpoint.as_ref().map(|p| &p.config);
    let ttl = profile.and_then(|p| p.cache_ttl_secs).unwrap_or(300);
    let minimum = profile
        .and_then(|p| p.cold_resume_min_tokens)
        .unwrap_or(20000);
    let tokens = {
        let j = state.control.journal.lock().unwrap();
        j.histories
            .get(id)
            .into_iter()
            .flatten()
            .rev()
            .find_map(|e| match e {
                HistoryEvent::Usage { usage } if usage.kind == "agent" => Some(
                    usage
                        .context_used
                        .or(usage.tokens.input_tokens)
                        .or(usage.input_estimate)
                        .unwrap_or(0)
                        .saturating_add(usage.tokens.output_tokens.unwrap_or(0)),
                ),
                _ => None,
            })
            .unwrap_or_else(|| checkpoint.as_ref().map_or(0, |p| p.request.input_estimate))
    };
    let fresh = requested_context == "fresh"
        || (requested_context == "auto" && idle > ttl as f64 && tokens > minimum);
    let reason = if requested_context != "auto" {
        format!("explicit {requested_context}")
    } else if fresh {
        format!(
            "cache likely cold (idle {idle:.0}s > {ttl}s); {tokens} input tokens > {minimum}; started a fresh context with history access"
        )
    } else {
        format!(
            "resumed context: idle {idle:.0}s, cache TTL {ttl}s, {tokens} input tokens (fresh threshold {minimum})"
        )
    };
    if fresh {
        seed_cold_context(state, id, &execution)?;
    }
    reservation.armed = false;
    launch_child(
        state,
        ChildLaunch {
            prompt,
            agent,
            execution,
            checkpoint,
            store,
            idle_seconds: Some(idle),
            context_decision: Some((if fresh { "fresh" } else { "resume" }.into(), reason)),
            steering: args["__discussion"] == true,
        },
        cancel,
    )
    .await
}
fn seed_cold_context(
    state: &State,
    id: &str,
    execution: &AgentExecution,
) -> Result<(), EngineError> {
    let names = state.control.tool_names(id);
    let (frozen, summary) = {
        let j = state.control.journal.lock().unwrap();
        let history = j.histories.get(id).map(Vec::as_slice).unwrap_or(&[]);
        let frozen = history
            .iter()
            .rev()
            .find(|e| matches!(e, HistoryEvent::PromptContext { .. }))
            .cloned();
        let report = history
            .iter()
            .rev()
            .find_map(|e| match e {
                HistoryEvent::Assistant {
                    text, tool_calls, ..
                } if tool_calls.is_empty() => Some(text.clone()),
                HistoryEvent::TurnError { error } => Some(format!("Error: {error}")),
                _ => None,
            })
            .unwrap_or_else(|| "No final report was recorded.".into());
        let summary = format!(
            "Cold follow-up.\nOriginal task: {}\nFinal report: {}\n{}",
            execution.original_task,
            report,
            names.rewrite(&crate::session_tools::earlier_context_footer(&j, id))
        );
        (frozen, summary)
    };
    let mut events = Vec::new();
    if let Some(frozen) = frozen {
        events.push(frozen);
    }
    events.push(HistoryEvent::Compaction { summary });
    let contract = state.control.contract(id);
    events.push(HistoryEvent::Goal {
        contract: contract.clone(),
    });
    events.push(HistoryEvent::SystemReminder {
        text: format!(
            "Authoritative current criteria: {}",
            serde_json::to_string(&contract).unwrap()
        ),
    });
    events.push(HistoryEvent::SystemReminder {
        text: state.control.decision_context(id),
    });
    let envelopes = state.control.journal.lock().unwrap().rotate(id, events)?;
    for envelope in envelopes {
        state.control.emit(EngineEvent::Committed { envelope });
    }
    Ok(())
}
struct ChildLaunch {
    context_decision: Option<(String, String)>,
    steering: bool,
    prompt: String,
    agent: AgentRecord,
    execution: AgentExecution,
    checkpoint: Option<model::ChildRequest>,
    store: ConfigStore,
    idle_seconds: Option<f64>,
}
async fn launch_child(
    state: &State,
    launch: ChildLaunch,
    cancel: &CancellationToken,
) -> Result<String, EngineError> {
    let ChildLaunch {
        steering,
        prompt,
        agent,
        execution,
        checkpoint,
        store,
        idle_seconds,
        context_decision,
    } = launch;
    let effort = execution.reasoning_effort.clone();
    let id = agent.agent_id.clone();
    let mut reservation = AgentReservation {
        control: &state.control,
        id: id.clone(),
        armed: true,
    };
    let model_id = agent
        .model
        .clone()
        .ok_or_else(|| EngineError::bad("child model missing"))?;
    let cwd = execution.cwd.clone();
    let remaining = execution.remaining_depth;
    let background = execution.background;
    state.tools.launched_child(&id);
    let token = cancel.child_token();
    let barrier = Arc::new(CommitBarrier::default());
    let child = Arc::new(AgentRun {
        parent: state.agent_id.clone(),
        cancel: token.clone(),
        barrier: barrier.clone(),
        result: Mutex::new(None),
        detached_for_question: AtomicBool::new(false),
        done: tokio::sync::Notify::new(),
        task: tokio::sync::Mutex::new(None),
    });
    let control = state.control.clone();
    let fresh = context_decision
        .as_ref()
        .is_some_and(|(mode, _)| mode == "fresh");
    let anchor = control
        .journal
        .lock()
        .unwrap()
        .histories
        .get(&id)
        .and_then(|history| {
            history.iter().rev().find_map(|event| {
                if let HistoryEvent::Usage { usage } = event {
                    (usage.kind == "agent" && !usage.partial)
                        .then(|| usage.tokens.input_tokens.zip(usage.input_estimate))
                        .flatten()
                } else {
                    None
                }
            })
        });
    control.watch_reviewer_guidance(&cwd)?;
    let mut child_state = State {
        store,
        tools: state
            .tools
            .fork_for_agent(id.clone(), control.breadcrumb(&id), cwd.clone()),
        gate: state.gate.clone(),
        config: state.config.clone(),
        control: control.clone(),
        cwd,
        anchor: if fresh { None } else { anchor },
        agent_id: id.clone(),
        remaining_depth: remaining,
        model_override: Some(model_id),
        effort_override: effort,
        compact_request: Arc::new(Mutex::new(None)),
        job_id: None,
        script_depth: 0,
        parent_call_id: None,
        warned_context: checkpoint.as_ref().is_some_and(|p| p.warned_context),
        compacted_estimate: if fresh {
            None
        } else {
            checkpoint.as_ref().and_then(|p| p.compacted_estimate)
        },
        child_profile: checkpoint,
        continuing: idle_seconds.is_some() && !fresh,
        guidance_turn: control.guidance_turn(&id),
        guidance_notices: Arc::default(),
        context_state: context::BoundaryState::default(),
        active_model: None,
    };
    // Finish fallible setup before publishing a job or an unfinished runtime child.
    let target = scheduler::Target::model(&child_state.model()?);
    let job = if background {
        Some(state.control.start_job(
            &state.agent_id,
            "task",
            prompt.clone(),
            Some(id.clone()),
            token.clone(),
            barrier.clone(),
        )?)
    } else {
        None
    };
    state
        .control
        .runtime
        .agents
        .lock()
        .unwrap()
        .insert(id.clone(), child.clone());
    let job_id = job.as_ref().map(|j| j.id.clone());
    child_state.job_id = job_id.clone();
    let turn = Uuid::new_v4().to_string();
    control
        .journal
        .lock()
        .unwrap()
        .activities
        .insert(id.clone(), (turn, None));
    let result_context = context_decision.clone();
    let run_handle = child.clone();
    let run_id = id.clone();
    let slot = control
        .scheduler
        .register(id.clone(), id.clone(), target, Some(&state.agent_id));
    let task = tokio::spawn(async move {
        let _slot = slot;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let relay_control = control.clone();
        let relay_id = run_id.clone();
        let relay = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                relay_control.emit_for(&relay_id, event);
            }
        });
        let input = if steering {
            let _ = control.commit(
                &run_id,
                HistoryEvent::Message {
                    from: "user".into(),
                    to: run_id.clone(),
                    text: prompt.clone(),
                },
            );
            control.emit(EngineEvent::AgentMessage {
                from: "user".into(),
                to: run_id.clone(),
                text: prompt,
                breadcrumb: control.breadcrumb(&run_id),
            });
            None
        } else {
            Some((prompt, vec![]))
        };
        let contract = control.contract(&run_id);
        if contract.criteria().next().is_some() || contract.roadblock().is_some() {
            let _ = control.commit(
                &run_id,
                HistoryEvent::SystemReminder {
                    text: format!(
                        "Use criteria only for multi-step work or work needing proof. If your parent passed no criteria, do this task and answer without creating criteria. Passed criteria are parent-owned; update and meet them with evidence. Only long-running children need partial reports. Your contract: {}",
                        serde_json::to_string(&contract).unwrap()
                    ),
                },
            );
        }
        let mut result = run(&mut child_state, input, &tx, &token, barrier.clone()).await;
        if result.is_ok()
            && !control.is_finalizing(&run_id)
            && control.contract(&run_id).review
            && control.contract(&run_id).has_completion_criteria()
            && control.contract(&run_id).roadblock().is_none()
            && !token.is_cancelled()
        {
            let policy = child_state.store.config().goal.clone();
            for round in 0..policy.max_review_rounds {
                match crate::goal_review::panel(&child_state, &token, barrier.clone()).await {
                    Ok((gaps, _)) if gaps.is_empty() => break,
                    Ok((gaps, next)) => {
                        if round + 1 >= policy.max_review_rounds {
                            let mut c = control.contract(&run_id);
                            c.roadblocks.push(crate::goal::Roadblock {
                                id: Uuid::new_v4().to_string(),
                                criterion_id: None,
                                reason: format!("Child review cap reached: {}", gaps.join("; "))
                                    .chars()
                                    .take(4096)
                                    .collect(),
                                options: vec![],
                                resolution: None,
                                note: None,
                                created: sessions::now(),
                                updated: sessions::now(),
                            });
                            if let Err(error) = control.save_contract(&run_id, c) {
                                result = Err(error);
                            }
                            break;
                        }
                        let _ = control.commit(
                            &run_id,
                            HistoryEvent::SystemReminder {
                                text: format!(
                                    "Goal review found gaps: {}. Next step: {}",
                                    serde_json::json!(gaps),
                                    next.unwrap_or_default()
                                ),
                            },
                        );
                        result = run(&mut child_state, None, &tx, &token, barrier.clone()).await;
                        if result.is_err() || control.contract(&run_id).roadblock().is_some() {
                            break;
                        }
                    }
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
        }
        if !token.is_cancelled()
            && control.is_finalizing(&run_id)
            && !result.as_ref().is_ok_and(|reason| reason == "finalized")
        {
            result = run(&mut child_state, None, &tx, &token, barrier.clone()).await;
        }
        let final_text = child_state
            .history()
            .iter()
            .rev()
            .find_map(|e| match e {
                HistoryEvent::Assistant {
                    text, tool_calls, ..
                } => Some(if tool_calls.is_empty() {
                    text.clone()
                } else {
                    String::new()
                }),
                _ => None,
            })
            .unwrap_or_default();
        if result.is_ok()
            && final_text.trim().is_empty()
            && !control.is_finalizing(&run_id)
            && !token.is_cancelled()
            && control.contract(&run_id).roadblock().is_none()
        {
            result = Err(EngineError::bad(
                "Child returned an empty final report after one reminder",
            ));
        }
        if let Err(error) = &result {
            control.emit_for(
                &run_id,
                EngineEvent::Error {
                    error: error.clone(),
                },
            );
        }
        control.finish_owned(&run_id).await;
        drop(tx);
        let _ = relay.await;
        let status = if token.is_cancelled() {
            "killed"
        } else if control.contract(&run_id).roadblock().is_some() {
            "roadblocked"
        } else if result.is_ok() {
            "done"
        } else {
            "failed"
        };
        let mut report = match &result {
            Err(e) => format!("Error: {e}"),
            Ok(_) => final_text,
        };
        if status != "done" || report.trim().is_empty() {
            report.push_str(&format!(
                "\n\nPartial progress (from the child's durable journal):\n{}",
                crate::progress::facts(&control, &run_id)
            ));
        }
        report.push_str(&control.contract(&run_id).report());
        report.push_str(&control.flat_question_report(&run_id));
        let transcript = control
            .journal
            .lock()
            .unwrap()
            .dir
            .join("agents")
            .join(&run_id)
            .join("contexts")
            .to_string_lossy()
            .into_owned();
        let _ = control.commit(
            &run_id,
            HistoryEvent::AgentReport {
                report: report.clone(),
                transcript: transcript.clone(),
            },
        );
        if control.is_finalizing(&run_id) {
            let _ = control.commit(&run_id, HistoryEvent::AgentFinalized);
        }
        let _ = control.agent_status(&run_id, status);
        control.journal.lock().unwrap().activities.remove(&run_id);
        control.emit(EngineEvent::AgentFinished {
            agent_id: run_id.clone(),
            status: status.into(),
            report: report.clone(),
            transcript: transcript.clone(),
            breadcrumb: control.breadcrumb(&run_id),
        });
        if let Some(job) = job {
            let _ = job.append(report.as_bytes());
            let _ = control.finish_job(&job.id, status, report.clone(), Some(transcript.clone()));
        }
        let detached = {
            let mut result = run_handle.result.lock().unwrap();
            *result = Some(Ok(
                json!({"agentId":run_id,"status":status,"report":report,"transcript":transcript,"idleSeconds":idle_seconds,"context":result_context.as_ref().map(|c| &c.0),"contextReason":result_context.as_ref().map(|c| &c.1)}),
            ));
            run_handle.detached_for_question.load(Ordering::Acquire)
        };
        if detached {
            let _ = control.message(
                &run_id,
                &run_handle.parent,
                format!("Child {run_id} {status}: {report}\nTranscript: {transcript}"),
            );
        }
        run_handle.done.notify_waiters();
        control.changed.notify_waiters();
    });
    *child.task.lock().await = Some(task);
    reservation.armed = false;
    if let Some(job_id) = job_id {
        return Ok(
            json!({"agentId":id,"jobId":job_id,"status":"running","idleSeconds":idle_seconds,"context":context_decision.as_ref().map(|c| &c.0),"contextReason":context_decision.as_ref().map(|c| &c.1)})
                .to_string(),
        );
    }
    state.control.agent_status(&state.agent_id, "waiting")?;
    let result = child.wait_foreground(&state.control, &id, cancel).await;
    if !state.tools.in_background() {
        state.control.agent_status(&state.agent_id, "running")?;
    }
    result.map(|v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_child_preparation_publishes_no_pending_run_or_job() {
        for bad_guidance in [false, true] {
            for background in [false, true] {
                let (temp, store) = crate::test_support::store(json!({}));
                let session =
                    Session::create(store.clone(), temp.path().to_owned(), Default::default())
                        .unwrap();
                let cwd = temp.path().join("child");
                std::fs::create_dir(&cwd).unwrap();
                if bad_guidance {
                    std::fs::create_dir_all(cwd.join(".cockpit-code/reviewer-guidance.md"))
                        .unwrap();
                }
                let mut agent = session.control.journal.lock().unwrap().agents["root"].clone();
                agent.agent_id = "build-child".into();
                agent.parent_id = Some("root".into());
                agent.model = Some("missing/model".into());
                agent.status = "running".into();
                session
                    .control
                    .journal
                    .lock()
                    .unwrap()
                    .register(agent.clone())
                    .unwrap();
                let execution: AgentExecution = serde_json::from_value(json!({
                    "launchId":"launch-1", "agentId":agent.agent_id, "cwd":cwd,
                    "title":"", "modelPolicy":[],
                    "remainingDepth":null, "originalTask":"task", "task":"task", "background":background
                })).unwrap();
                let result = launch_child(
                    &*session.state.lock().await,
                    ChildLaunch {
                        steering: false,
                        prompt: "task".into(),
                        agent,
                        execution,
                        checkpoint: None,
                        store,
                        idle_seconds: None,
                        context_decision: None,
                    },
                    &CancellationToken::new(),
                )
                .await;
                assert!(result.is_err());
                assert!(!session.control.runtime.has_unfinished());
                assert!(!session.control.runtime.children_pending("root"));
                assert!(session.control.runtime.agents.lock().unwrap().is_empty());
                assert!(session.control.runtime.jobs.lock().unwrap().is_empty());
                {
                    let journal = session.control.journal.lock().unwrap();
                    assert_eq!(journal.agents["build-child"].status, "failed");
                    assert!(journal.jobs.is_empty());
                }
                tokio::time::timeout(Duration::from_secs(5), session.shutdown())
                    .await
                    .unwrap()
                    .unwrap();
            }
        }
    }
}
