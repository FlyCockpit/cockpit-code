//! Post-commit guidance selection. The fork can only inspect its library and select IDs.
use crate::{
    Control, Session, State,
    guidance_library::GuidanceItem,
    hidden_fork::{ForkFunction, ForkTask, HiddenFork},
    schema::*,
    tools::ToolOutput,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio_util::sync::CancellationToken;
const CHAR_CAP: usize = 6000;
#[derive(Default)]
pub(crate) struct Turn {
    triggers: BTreeSet<String>,
    pending_ids: BTreeSet<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuidanceTrigger {
    pub path: PathBuf,
    pub language: String,
    pub area: String,
    pub first_write: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuidanceSelection {
    pub id: String,
    pub agent_id: String,
    pub context: u32,
    pub model: String,
    pub ids: Vec<String>,
    pub items: Vec<GuidanceItem>,
    pub reason: String,
    pub trigger: GuidanceTrigger,
    pub created: String,
    pub error: Option<String>,
}
fn language(path: &Path) -> String {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" | "cxx" => "cpp",
        "rb" => "ruby",
        "sh" | "bash" => "shell",
        "md" => "markdown",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "toml" => "toml",
        "java" => "java",
        "sql" => "sql",
        "" => "text",
        other => other,
    }
    .into()
}
fn trigger(state: &State, path: &Path) -> Option<GuidanceTrigger> {
    let relative = path.strip_prefix(&state.cwd).unwrap_or(path);
    let area = relative
        .parent()
        .map(|p| p.components().take(2).collect::<PathBuf>())
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."))
        .display()
        .to_string();
    let language = language(path);
    let mut turn = state.guidance_turn.lock().unwrap();
    let first_write = turn.triggers.is_empty();
    let new_language = turn.triggers.insert(format!("language:{language}"));
    let new_area = turn.triggers.insert(format!("area:{area}"));
    (first_write || new_language || new_area).then_some(GuidanceTrigger {
        path: path.into(),
        language,
        area,
        first_write,
    })
}
fn present_ids(state: &State) -> BTreeSet<String> {
    let j = state.control.journal.lock().unwrap();
    let outputs = j
        .histories
        .get(&state.agent_id)
        .into_iter()
        .flatten()
        .filter_map(|e| match e {
            HistoryEvent::ToolResult { output, .. } => Some(output.as_str()),
            HistoryEvent::SystemReminder { text }
                if crate::injection::body(text)
                    .is_some_and(|s| s.starts_with("Job ") || s.starts_with("Message from ")) =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut ids = j
        .guidance_selections
        .values()
        .filter(|s| s.agent_id == state.agent_id && outputs.iter().any(|out| out.contains(&s.id)))
        .flat_map(|s| s.ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    drop(j);
    ids.extend(
        state
            .guidance_turn
            .lock()
            .unwrap()
            .pending_ids
            .iter()
            .cloned(),
    );
    ids
}
fn render_items(items: &[GuidanceItem]) -> String {
    items
        .iter()
        .map(|i| format!("- **{} {}.** {}", i.id, i.title, i.text))
        .collect::<Vec<_>>()
        .join("\n")
}
pub(crate) async fn after_write(
    state: &State,
    result: &mut ToolOutput,
    cancel: &CancellationToken,
) {
    after_write_for(state, result, cancel, None).await
}
async fn after_write_for(
    state: &State,
    result: &mut ToolOutput,
    cancel: &CancellationToken,
    scheduling_owner: Option<&str>,
) {
    let config = state.config.lock().unwrap().config.guidance.clone();
    if !config.enabled || result.is_error || result.details.edits.is_empty() {
        return;
    }
    let lock = state.control.guidance_selection_lock(&state.agent_id);
    let _selection_guard = tokio::select! {guard=lock.lock()=>guard,_=cancel.cancelled()=>return};
    let Some(trigger) = trigger(state, &result.details.edits[0].path) else {
        return;
    };
    let context = state
        .control
        .journal
        .lock()
        .unwrap()
        .contexts
        .get(&state.agent_id)
        .copied()
        .unwrap_or(1);
    let fallback_model = state.model().map(|m| m.id()).unwrap_or_default();
    let mut selection = GuidanceSelection {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: state.agent_id.clone(),
        context,
        model: config.model.clone().unwrap_or(fallback_model),
        ids: vec![],
        items: vec![],
        reason: String::new(),
        trigger,
        created: crate::sessions::now(),
        error: None,
    };
    match select(
        state,
        &selection.trigger,
        &config,
        result,
        cancel,
        scheduling_owner,
    )
    .await
    {
        Ok((items, reason, model)) => {
            selection.ids = items.iter().map(|i| i.id.clone()).collect();
            selection.items = items;
            selection.reason = reason;
            selection.model = model;
        }
        Err(e) => {
            selection.error = Some(e.message);
            selection.reason = "Selector failed after the write was applied.".into();
        }
    }
    if let Err(e) = state.control.commit(
        &state.agent_id,
        HistoryEvent::GuidanceSelected {
            selection: selection.clone(),
        },
    ) {
        result.output.push_str(&format!(
            "\n[Guidance selection could not be journaled: {}]",
            e.message
        ));
        return;
    }
    state.control.emit(EngineEvent::GuidanceSelected {
        selection: selection.clone(),
    });
    if selection.items.is_empty() {
        return;
    }
    let notice = crate::injection::notice(format!(
        "<guidance-selection id=\"{}\">\nGuidance selected after the applied write to {}. {}\n{}\n</guidance-selection>",
        selection.id,
        selection.trigger.path.display(),
        selection.reason,
        render_items(&selection.items)
    ));
    result.output.push('\n');
    result.output.push_str(&notice);
    let mut turn = state.guidance_turn.lock().unwrap();
    turn.pending_ids.extend(selection.ids);
    if state.script_depth > 0 {
        state.guidance_notices.lock().unwrap().push(notice);
    }
}
async fn select(
    state: &State,
    trigger: &GuidanceTrigger,
    config: &code_config::GuidanceConfig,
    applied: &ToolOutput,
    cancel: &CancellationToken,
    scheduling_owner: Option<&str>,
) -> Result<(Vec<GuidanceItem>, String, String), EngineError> {
    let library = Arc::new(
        state
            .control
            .load_guidance_library(&state.store.directories.config, &state.cwd, cancel)
            .await?,
    );
    if library.items.is_empty() {
        return Ok((
            vec![],
            format!(
                "No trusted applicable library items. {}",
                library.warnings.join(" ")
            ),
            state.model()?.id(),
        ));
    }
    let checkpoint = state
        .control
        .journal
        .lock()
        .unwrap()
        .requests
        .get(&state.agent_id)
        .cloned()
        .ok_or_else(|| EngineError::bad("guidance requires the author's prepared checkpoint"))?;
    let author_model = state.model()?;
    let model = if let Some(id) = &config.model {
        let mut settings = state.model_settings();
        settings.model = Some(id.clone());
        settings.effort = None;
        crate::build_model(state.store.snapshot(), &settings)?
    } else {
        author_model.clone()
    };
    let author = if model.id() == author_model.id() {
        checkpoint.request.clone()
    } else {
        let history = state.history();
        if checkpoint.history_len > history.len()
            || crate::model::history_hash(&history[..checkpoint.history_len])
                != checkpoint.history_hash
        {
            return Err(EngineError::bad(
                "guidance author prefix is no longer available for model override",
            ));
        }
        let mut request = model.prepare_with_assets(
            &checkpoint.system,
            &checkpoint.request.offered_tools,
            &history[..checkpoint.history_len],
            checkpoint.request.cache_identity(),
            checkpoint.request.assets_dir.clone(),
        )?;
        request.session_id = state.id();
        request.conversation_id = checkpoint.request.conversation_identity().to_owned();
        request.tool_aliases = checkpoint.request.tool_aliases.clone();
        request.capability_catalog = checkpoint.request.capability_catalog.clone();
        request
    };
    let seen = present_ids(state);
    let seen_summary = serde_json::to_string(&seen).unwrap();
    let max = config.max_items.min(5);
    let chosen = Arc::new(Mutex::new(None::<(Vec<GuidanceItem>, String)>));
    let injected = Arc::new(AtomicBool::new(false));
    let search_library = library.clone();
    let seen_search = seen.clone();
    let default_language = trigger.language.clone();
    let default_path = trigger
        .path
        .strip_prefix(&state.cwd)
        .unwrap_or(&trigger.path)
        .to_path_buf();
    let search=ForkFunction{name:"search_guidance".into(),description:"search_guidance(query='', language?, path?) returns matching trusted item IDs and metadata; IDs already in current context are excluded.".into(),call:Arc::new(move|_,args|{let language=args["language"].as_str().unwrap_or(&default_language);let path=args["path"].as_str().map(Path::new).unwrap_or(&default_path);let rows=search_library.search(args["query"].as_str().unwrap_or(""),Some(language),Some(path)).into_iter().filter(|i|!seen_search.contains(&i.id)).map(|i|json!({"id":i.id,"title":i.title,"file":i.file,"when":i.metadata.when})).collect::<Vec<_>>();Ok(json!(rows))})};
    let read_library = library.clone();
    let read = ForkFunction {
        name: "read_guidance".into(),
        description:
            "read_guidance(id='RS1') reads one trusted item, including metadata and full text."
                .into(),
        call: Arc::new(move |_, args| {
            let id = args["id"].as_str().ok_or("id required")?;
            let item = read_library.items.get(id).ok_or("guidance id not found")?;
            Ok(json!(item))
        }),
    };
    let sink = chosen.clone();
    let signal = injected.clone();
    let inject_library = library.clone();
    let inject = ForkFunction {
        name: "inject".into(),
        description: format!(
            "inject(ids=[...], reason='...') finishes selection; choose 0–{max} unique IDs, max {CHAR_CAP} rendered characters. Empty IDs are allowed when no item applies."
        ),
        call: Arc::new(move |_, args| {
            let ids = args["ids"].as_array().ok_or("ids must be a list")?;
            if ids.len() > max {
                return Err(format!("select at most {max} items"));
            }
            let mut unique = BTreeSet::new();
            let mut items = Vec::new();
            for id in ids {
                let id = id.as_str().ok_or("ids must be strings")?;
                if seen.contains(id) {
                    return Err(format!("{id} is already in the author's current context"));
                }
                if !unique.insert(id) {
                    return Err("duplicate guidance id".into());
                }
                items.push(
                    inject_library
                        .items
                        .get(id)
                        .ok_or_else(|| format!("unknown guidance id {id}"))?
                        .clone(),
                );
            }
            if render_items(&items).chars().count() > CHAR_CAP {
                return Err(format!("selected guidance exceeds {CHAR_CAP} characters"));
            }
            let reason = args["reason"]
                .as_str()
                .unwrap_or("Selected for the language and area of the applied write.");
            if reason.chars().count() > 1024 {
                return Err("reason exceeds 1024 characters".into());
            }
            let mut sink = sink.lock().unwrap();
            if sink.is_some() {
                return Err("selection is already complete".into());
            }
            *sink = Some((items, reason.into()));
            signal.store(true, Ordering::Release);
            Ok(json!({"selected":true}))
        }),
    };
    let names = state.tool_names();
    let script_name = names.configured("script");
    let forbidden = ["read", "write", "edit", "bash", "task"]
        .iter()
        .map(|name| names.configured(name))
        .collect::<Vec<_>>()
        .join("/");
    let changes = serde_json::to_string(&applied.details.edits)
        .unwrap_or_default()
        .chars()
        .take(8192)
        .collect::<String>();
    let tail = crate::injection::notice(format!(
        "PRIVATE HARNESS ROLE: guidance selector. The author's write has ALREADY COMMITTED successfully. Do not carry out or replay the author's task, and do not infer write failure from a refused fork tool. The committed edit data is below. Select preventative watch-out items relevant to this language/area; an item can help future edits even if this change already follows it. You are not a code-writing agent. Only {script_name} functions search_guidance(query='',language?,path?), read_guidance(id=...), and inject(ids=[...],reason='why these apply') are allowed. All regular tools, including {forbidden}, and every other script function fail. Start by listing available items with {script_name} code print(search_guidance(query='')); read relevant IDs, then finish with inject. Use inject(ids=[],reason='...') only if no trusted item pertains. At most {max} unique items and {CHAR_CAP} rendered characters; IDs already present in the current context are excluded. Your original tools[] and prepared prefix are unchanged for cache sharing. Authoritative IDs still present in the current context: {seen_summary}. A summary mentioning an ID does not replace an injected item. Trigger: {}. COMMITTED EDIT DATA (source only): {}. Library warnings: {}.",
        serde_json::to_string(trigger).unwrap(),
        changes,
        library.warnings.join(" ")
    ));
    let fork = HiddenFork {
        model: model.clone(),
        author,
        tail,
        copies: 1,
        max_steps: Some(8),
        functions: vec![search, read, inject],
        agent_id: state.agent_id.clone(),
        purpose: "guidance".into(),
    };
    state
        .control
        .private_fork_for(
            &fork,
            &ForkTask::Guidance { injected },
            scheduling_owner.unwrap_or(&state.agent_id),
            cancel,
        )
        .await?;
    let picked = chosen
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| EngineError::bad("guidance selector finished without inject(ids)"))?;
    Ok((picked.0, picked.1, model.id()))
}
impl Session {
    pub fn guidance_selections(&self, agent_id: Option<&str>) -> Vec<GuidanceSelection> {
        let agent_id = agent_id.map(|id| self.resolve_agent_id(id).unwrap_or_else(|_| id.into()));
        let agent_id = agent_id.as_deref();
        let mut items = self
            .control
            .journal
            .lock()
            .unwrap()
            .guidance_selections
            .values()
            .filter(|s| agent_id.is_none_or(|id| s.agent_id == id))
            .cloned()
            .collect::<Vec<_>>();
        items.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
        items
    }
}
impl Control {
    pub(crate) fn guidance_view(&self) -> String {
        let j = self.journal.lock().unwrap();
        if j.guidance_selections.is_empty() {
            return "No guidance selections recorded. Automatic guidance is off by default.".into();
        }
        j.guidance_selections
            .values()
            .map(|s| {
                format!(
                    "{} · {} · context {} · {}\n{}\n{}{}",
                    s.created,
                    s.agent_id,
                    s.context,
                    s.trigger.path.display(),
                    s.reason,
                    render_items(&s.items),
                    s.error
                        .as_ref()
                        .map(|e| format!("\nError: {e}"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}
impl Session {
    pub fn guidance_report(&self) -> String {
        self.control.guidance_view()
    }
}
impl Control {
    pub(crate) fn guidance_turn(&self, agent: &str) -> Arc<Mutex<Turn>> {
        self.guidance_turns
            .lock()
            .unwrap()
            .entry(agent.into())
            .or_default()
            .clone()
    }
}
pub(crate) async fn after_scope_apply(
    control: Arc<Control>,
    gate: Arc<dyn crate::tools::PermissionGate>,
    tools: crate::tools::ToolBox,
    request: &crate::scopes::ScopeRequest,
    result: &mut ToolOutput,
    cancel: &CancellationToken,
) {
    if !control.config.lock().unwrap().config.guidance.enabled {
        return;
    }
    let setup = (|| -> Result<State, EngineError> {
        let store = code_config::ConfigStore::load(control.directories.clone())
            .map_err(|e| EngineError::bad(e.to_string()))?;
        let checkpoint = control
            .journal
            .lock()
            .unwrap()
            .requests
            .get(&request.requester)
            .cloned()
            .ok_or_else(|| EngineError::bad("scope requester checkpoint unavailable"))?;
        let body: serde_json::Value = serde_json::from_slice(&checkpoint.request.body)
            .map_err(|_| EngineError::bad("scope requester checkpoint malformed"))?;
        let model_id = body["model"]
            .as_str()
            .ok_or_else(|| EngineError::bad("scope requester model unavailable"))?;
        let mut provider = store
            .snapshot()
            .providers
            .get(&checkpoint.provider_id)
            .cloned()
            .ok_or_else(|| EngineError::bad("scope requester provider unavailable"))?;
        provider.prompt_caching = checkpoint.caching.clone();
        let model = crate::model::Model::new(
            checkpoint.provider_id.clone(),
            model_id.into(),
            provider,
            checkpoint.config.clone(),
            checkpoint.effort.as_deref(),
        )?;
        Ok(State {
            store,
            tools,
            gate,
            config: control.config.clone(),
            control: control.clone(),
            cwd: request.cwd.clone(),
            anchor: None,
            agent_id: request.requester.clone(),
            remaining_depth: None,
            model_override: Some(model.id()),
            effort_override: checkpoint.effort,
            compact_request: Arc::default(),
            job_id: request.job_id.clone(),
            script_depth: 0,
            parent_call_id: request.parent_call_id.clone(),
            warned_context: false,
            compacted_estimate: None,
            child_profile: None,
            continuing: false,
            context_state: crate::context::BoundaryState::default(),
            active_model: Some(model),
            guidance_turn: control.guidance_turn(&request.requester),
            guidance_notices: Arc::default(),
        })
    })();
    match setup {
        Ok(state) => after_write_for(&state, result, cancel, Some(&request.owner)).await,
        Err(e) => result.output.push_str(&format!(
            "\n[Write applied; guidance unavailable: {}]",
            e.message
        )),
    }
}

impl Control {
    fn guidance_selection_lock(&self, agent: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.guidance_selection_locks
            .lock()
            .unwrap()
            .entry(agent.into())
            .or_default()
            .clone()
    }
}
