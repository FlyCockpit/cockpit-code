//! Discussions and interactive checkpoint forks. Chat records are side files,
//! never ordinary agent history, even after reopening or resuming a session.
use crate::{
    Control, Session,
    hidden_fork::{ForkFunction, HiddenFork},
    model::{ChildRequest, PreparedRequest},
    read_only::ReadOnly,
    schema::*,
    sessions,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

const ALLOWED: &str = "Allowed actions: read, grep, glob, history, web_search, web_fetch, and script using those read-only functions plus answer_question(questionId, answers, summary).";
/// The `/btw` side chat (a question fork with no question).
const SIDE_ALLOWED: &str = "Allowed actions: read, grep, glob, history, web_search, web_fetch, and script using those read-only functions plus btw_list() (earlier /btw side chats of this session, including ones from before a compaction) and btw_search(chatId, query) (find specific information in one of them).";
fn allowed(side: bool) -> &'static str {
    if side { SIDE_ALLOWED } else { ALLOWED }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkChatMessage {
    pub role: String,
    pub text: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResultData>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionFork {
    pub id: String,
    pub question_id: String,
    pub asker: String,
    pub closed: bool,
    pub messages: Vec<ForkChatMessage>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    chat: QuestionFork,
    checkpoint: ChildRequest,
    request: PreparedRequest,
    cwd: PathBuf,
}
impl Control {
    pub(crate) fn discussing(&self, asker: &str) -> bool {
        self.journal
            .lock()
            .unwrap()
            .questions
            .values()
            .any(|q| q.asker == asker && q.discussing)
    }
    fn question_by_call(&self, id: &str) -> Result<QuestionRecord, EngineError> {
        self.journal
            .lock()
            .unwrap()
            .questions
            .values()
            .find(|q| q.id == id || q.call_id.as_deref() == Some(id))
            .cloned()
            .ok_or_else(|| EngineError::bad("question not found"))
    }
    fn fork_path(&self, id: &str) -> Result<PathBuf, EngineError> {
        if !(id.starts_with("question-fork-") || id.starts_with("side-chat-"))
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(EngineError::bad("invalid question fork ID"));
        }
        Ok(self
            .journal
            .lock()
            .unwrap()
            .dir
            .join("fork-chats")
            .join(format!("{id}.json")))
    }
    fn load_chat(&self, id: &str) -> Result<Record, EngineError> {
        sessions::read_json(&self.fork_path(id)?)
    }
    fn save_chat(&self, record: &Record) -> Result<(), EngineError> {
        let path = self.fork_path(&record.chat.id)?;
        let mut journal = self.journal.lock().unwrap();
        journal.ensure_persisted()?;
        journal
            .with_owner(Box::new(|| {
                std::fs::create_dir_all(path.parent().unwrap())?;
                sessions::write_json(&path, record).map_err(anyhow::Error::from)
            }))
            .map_err(|e| EngineError::bad(e.to_string()))
    }
    pub(crate) fn fork_status(&self, id: &str, status: &str) {
        if let Ok(record) = self.load_chat(id) {
            self.chat_event(&record, "status", status.into());
        }
    }
    fn chat_event(&self, record: &Record, kind: &str, text: String) {
        self.emit(fork_event(record, kind, text));
    }
    /// Save a chat's record, then send `event` about what changed in it. A
    /// reopen reads the record and sends its snapshot under the same lock,
    /// so the view gets each change either in the snapshot or after it,
    /// never both and never neither.
    fn saved_chat_event(&self, record: &Record, event: EngineEvent) -> Result<(), EngineError> {
        let _io = self.chat_io.lock().unwrap();
        self.save_chat(record)?;
        self.emit(event);
        Ok(())
    }
}
fn fork_event(record: &Record, kind: &str, text: String) -> EngineEvent {
    EngineEvent::ForkChat {
        fork_id: record.chat.id.clone(),
        question_id: record.chat.question_id.clone(),
        asker: record.chat.asker.clone(),
        kind: kind.into(),
        text,
        tool_call: None,
        tool_result: None,
        messages: if kind == "opened" {
            record.chat.messages.clone()
        } else {
            vec![]
        },
    }
}
impl Control {
    /// This agent's `/btw` side chats, oldest first (by forked context,
    /// then first message).
    fn side_chats(&self, agent: &str) -> Vec<Record> {
        let dir = self.journal.lock().unwrap().dir.join("fork-chats");
        let mut out: Vec<Record> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| sessions::read_json::<Record>(&e.path()).ok())
            .filter(|r| r.chat.question_id.is_empty() && r.chat.asker == agent)
            .collect();
        out.sort_by(|a, b| {
            (
                a.checkpoint.context,
                a.chat.messages.first().map(|m| m.ts.clone()),
            )
                .cmp(&(
                    b.checkpoint.context,
                    b.chat.messages.first().map(|m| m.ts.clone()),
                ))
        });
        out
    }
}

/// `btw_list()`: every side chat of the agent with its exchanges, short.
fn btw_list(control: &Control, agent: &str, current: &str) -> Value {
    let short = |t: &str, n: usize| -> String {
        let t = t.trim();
        if t.chars().count() > n {
            t.chars().take(n).collect::<String>() + "…"
        } else {
            t.to_string()
        }
    };
    let chats: Vec<Value> = control
        .side_chats(agent)
        .into_iter()
        .map(|r| {
            let mut exchanges = vec![];
            let mut question: Option<&ForkChatMessage> = None;
            for m in &r.chat.messages {
                match m.role.as_str() {
                    "user" => question = Some(m),
                    "assistant" if !m.text.trim().is_empty() => {
                        if let Some(q) = question.take() {
                            exchanges.push(json!({
                                "ts": q.ts,
                                "question": short(&q.text, 300),
                                "answer": short(&m.text, 300),
                            }));
                        }
                    }
                    _ => {}
                }
            }
            if let Some(q) = question {
                exchanges
                    .push(json!({"ts": q.ts, "question": short(&q.text, 300), "answer": null}));
            }
            json!({
                "chatId": r.chat.id,
                "context": r.checkpoint.context,
                "current": r.chat.id == current,
                "exchanges": exchanges,
            })
        })
        .collect();
    json!({ "chats": chats })
}

/// `btw_search(chatId, query)`: the messages of one side chat matching any
/// query word (all messages when the query is empty), full text, capped.
fn btw_search(control: &Control, agent: &str, args: &Value) -> Result<Value, String> {
    let id = args["chatId"]
        .as_str()
        .or_else(|| args["chat_id"].as_str())
        .ok_or("btw_search needs chatId (from btw_list)")?;
    let record = control
        .side_chats(agent)
        .into_iter()
        .find(|r| r.chat.id == id)
        .ok_or("no such side chat for this agent; call btw_list()")?;
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut budget = 16 * 1024;
    let mut hits = vec![];
    for (i, m) in record.chat.messages.iter().enumerate() {
        if !matches!(m.role.as_str(), "user" | "assistant") || m.text.trim().is_empty() {
            continue;
        }
        let lower = m.text.to_lowercase();
        if !words.is_empty() && !words.iter().any(|w| lower.contains(w)) {
            continue;
        }
        let text: String = m.text.chars().take(budget).collect();
        budget = budget.saturating_sub(text.len());
        hits.push(json!({"index": i, "role": m.role, "ts": m.ts, "text": text}));
        if budget == 0 {
            hits.push(json!({"truncated": true}));
            break;
        }
    }
    Ok(json!({"chatId": id, "matches": hits}))
}

impl Session {
    /// Answer with a discuss marker and park at the next boundary without a model call.
    pub fn discuss_question(&self, id: &str) -> Result<(), EngineError> {
        let envelope = {
            let mut j = self.control.journal.lock().unwrap();
            let q = j
                .questions
                .values()
                .find(|q| q.id == id || q.call_id.as_deref() == Some(id))
                .ok_or_else(|| EngineError::bad("question not found"))?;
            if q.answer.is_some() {
                return Err(EngineError::bad("question already answered"));
            }
            let (asker, id) = (q.asker.clone(), q.id.clone());
            j.append(&asker, HistoryEvent::QuestionDiscussed { id })?
        };
        self.control.emit(EngineEvent::Committed { envelope });
        self.control.changed.notify_waiters();
        Ok(())
    }
    pub(crate) fn resume_discussion(&self, asker: &str) {
        if self.control.discussing(asker) {
            let _ = self.control.commit(
                asker,
                HistoryEvent::DiscussionResumed {
                    asker: asker.into(),
                },
            );
            self.control.changed.notify_waiters();
        }
    }
    /// Opening or reopening does not use tokens. Call fork_chat_send to converse.
    pub fn open_question_fork(&self, id: &str) -> Result<String, EngineError> {
        self.control.check_owner()?;
        let q = self.control.question_by_call(id)?;
        let dir = self.control.journal.lock().unwrap().dir.join("fork-chats");
        let io = self.control.chat_io.lock().unwrap();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(mut record) = sessions::read_json::<Record>(&entry.path())
                    && record.chat.question_id == q.id
                {
                    if q.answer.is_some() && !record.chat.closed {
                        record.chat.closed = true;
                        self.control.save_chat(&record)?;
                    }
                    self.control.chat_event(&record, "opened", String::new());
                    return Ok(record.chat.id);
                }
            }
        }
        drop(io);
        if q.answer.is_some() {
            return Err(EngineError::bad(
                "question already answered and has no fork record",
            ));
        }
        let (checkpoint, cwd) = {
            let j = self.control.journal.lock().unwrap();
            (
                j.requests
                    .get(&q.asker)
                    .cloned()
                    .ok_or_else(|| EngineError::bad("asker has no prepared checkpoint yet"))?,
                j.executions
                    .get(&q.asker)
                    .map(|e| e.cwd.clone())
                    .unwrap_or_else(|| j.header.cwd.clone()),
            )
        };
        let model = chat_model(&self.config, || self.settings(), &checkpoint)?;
        let id = format!("question-fork-{}", uuid::Uuid::new_v4());
        let fork = HiddenFork {
            model,
            author: checkpoint.request.clone(),
            tail: crate::injection::notice(format!(
                "Discuss question set {} with the user: {}. This is a private interactive fork. {ALLOWED} All original tools and system instructions stay fixed; only the listed actions are permitted here. answer_question is available only inside script and closes this fork after delivering validated answers plus your short reasoning summary to the asker.",
                q.id,
                serde_json::to_string(&q.questions).unwrap()
            )),
            copies: 1,
            max_steps: None,
            functions: vec![],
            agent_id: q.asker.clone(),
            purpose: "question_fork".into(),
        };
        let record = Record {
            chat: QuestionFork {
                id: id.clone(),
                question_id: q.id,
                asker: q.asker,
                closed: false,
                messages: vec![],
            },
            request: fork.prepare()?,
            checkpoint,
            cwd,
        };
        self.control.save_chat(&record)?;
        self.control.chat_event(&record, "opened", String::new());
        Ok(id)
    }
    /// `/btw`: open (or reopen) the side chat of `agent`, a private
    /// interactive hidden fork with read-only tools. It has no
    /// question to answer and never enters any agent's context; its record
    /// persists with the session. Opening uses no tokens.
    pub fn open_side_chat(&self, agent: &str) -> Result<String, EngineError> {
        let agent = self.resolve_agent_id(agent)?;
        let agent = agent.as_str();
        self.control.check_owner()?;
        // The newest side chat is reused while the agent is still in
        // the context it forked from; after a compaction a fresh fork shares
        // the new context's cache, and earlier chats stay reachable through
        // btw_list / btw_search.
        let current = self
            .control
            .journal
            .lock()
            .unwrap()
            .requests
            .get(agent)
            .map(|r| r.context);
        {
            let _io = self.control.chat_io.lock().unwrap();
            if let Some(record) = self.control.side_chats(agent).pop()
                && current.is_none_or(|c| c <= record.checkpoint.context)
            {
                self.control.chat_event(&record, "opened", String::new());
                return Ok(record.chat.id);
            }
        }
        let (checkpoint, cwd) = {
            let j = self.control.journal.lock().unwrap();
            (
                j.requests.get(agent).cloned().ok_or_else(|| {
                    EngineError::bad(
                        "this agent has not made a model request yet; send it a message first",
                    )
                })?,
                j.executions
                    .get(agent)
                    .map(|e| e.cwd.clone())
                    .unwrap_or_else(|| j.header.cwd.clone()),
            )
        };
        let model = chat_model(&self.config, || self.settings(), &checkpoint)?;
        let id = format!("side-chat-{}", uuid::Uuid::new_v4());
        let fork = HiddenFork {
            model,
            author: checkpoint.request.clone(),
            tail: crate::injection::notice(format!(
                "Side chat with the user (/btw). This is a private interactive fork: nothing said here reaches you or any other agent afterwards, and you must not try to continue the main task from here. Answer the user's side questions about this session or the project. {SIDE_ALLOWED} All original tools and system instructions stay fixed; only the listed actions are permitted here."
            )),
            copies: 1,
            max_steps: None,
            functions: vec![],
            agent_id: agent.into(),
            purpose: "side_chat".into(),
        };
        let record = Record {
            chat: QuestionFork {
                id: id.clone(),
                question_id: String::new(),
                asker: agent.into(),
                closed: false,
                messages: vec![],
            },
            request: fork.prepare()?,
            checkpoint,
            cwd,
        };
        self.control.save_chat(&record)?;
        self.control.chat_event(&record, "opened", String::new());
        Ok(id)
    }
    pub fn question_fork(&self, id: &str) -> Result<QuestionFork, EngineError> {
        let mut chat = self.control.load_chat(id)?.chat;
        if chat.question_id.is_empty() {
            return Ok(chat);
        }
        if self
            .control
            .question_by_call(&chat.question_id)?
            .answer
            .is_some()
        {
            chat.closed = true;
        }
        Ok(chat)
    }
    pub fn fork_chat_send(&self, id: &str, text: impl Into<String>) -> Result<(), EngineError> {
        let text = text.into();
        if text.trim().is_empty() || text.len() > 256 * 1024 {
            return Err(EngineError::bad(
                "fork message must be nonempty and at most 256 KiB",
            ));
        }
        self.control.check_owner()?;
        let mut work = self.control.title_work.lock().unwrap();
        if work.closing {
            return Err(EngineError::bad("session is shutting down"));
        }
        work.tasks.retain(|task| !task.is_finished());
        let mut record = self.control.load_chat(id)?;
        let side = record.chat.question_id.is_empty();
        if record.chat.closed
            || (!side
                && self
                    .control
                    .question_by_call(&record.chat.question_id)?
                    .answer
                    .is_some())
        {
            return Err(EngineError::bad(
                "question fork is closed; its record can still be reopened",
            ));
        }
        let model = match chat_model(&self.config, || self.settings(), &record.checkpoint) {
            Ok(model) => model,
            Err(error) => {
                // The fork cannot answer now (its provider or model is
                // gone): the reason stays in the record for a reopen. A
                // reply still running owns the record and is left alone; one
                // that finished since the load above saved it, so the record
                // is read again.
                let _io = self.control.chat_io.lock().unwrap();
                if !self.control.fork_busy.lock().unwrap().contains(id) {
                    let mut record = self.control.load_chat(id)?;
                    record.chat.messages.push(error_message(&error));
                    self.control.save_chat(&record)?;
                }
                return Err(error);
            }
        };
        record.request = record.request.with_user_tail(&text)?;
        if !self.control.fork_busy.lock().unwrap().insert(id.into()) {
            return Err(EngineError::bad("question fork is already responding"));
        }
        record.chat.messages.push(ForkChatMessage {
            tool_call: None,
            tool_result: None,
            role: "user".into(),
            text: text.clone(),
            ts: sessions::now(),
        });
        let event = fork_event(&record, "user", text);
        if let Err(e) = self.control.saved_chat_event(&record, event) {
            self.control.fork_busy.lock().unwrap().remove(id);
            return Err(e);
        }
        let control = self.control.clone();
        let cancel = work.cancel.child_token();
        let weak = Arc::downgrade(&self._lifetime);
        let state = self.state.clone();
        let coordinator = self.coordinator.clone();
        let config = self.config.clone();
        let gate = self.gate.clone();
        let directories = self.directories.clone();
        work.tasks.push(tokio::spawn(async move {
            let result = converse(&control, &config, &gate, &model, &mut record, &cancel).await;
            // The error is part of the record: a reopened chat shows it.
            {
                let _io = control.chat_io.lock().unwrap();
                if let Err(error) = &result {
                    record.chat.messages.push(error_message(error));
                }
                let saved = control.save_chat(&record);
                for error in [result.err(), saved.err()].into_iter().flatten() {
                    control.chat_event(&record, "error", error.message);
                }
            }
            control.fork_busy.lock().unwrap().remove(&record.chat.id);
            control.chat_event(
                &record,
                if record.chat.closed { "closed" } else { "idle" },
                String::new(),
            );
            if let Some(lifetime) = weak.upgrade() {
                Session {
                    directories,
                    state,
                    control,
                    coordinator,
                    config,
                    gate,
                    _lifetime: lifetime,
                }
                .wake_answer_turn();
            }
        }));
        Ok(())
    }
}
/// A failure of the fork, as a row of its saved chat.
fn error_message(error: &EngineError) -> ForkChatMessage {
    ForkChatMessage {
        role: "error".into(),
        text: error.message.clone(),
        ts: sessions::now(),
        tool_call: None,
        tool_result: None,
    }
}
/// Builds the fork's model from its checkpoint. The config lock is taken after the body parses and held
/// through the exclusive-model check, and `settings` (which locks the journal) runs only at that check.
fn chat_model(
    config: &Mutex<code_config::ConfigStore>,
    settings: impl FnOnce() -> crate::sessions::SessionSettings,
    checkpoint: &ChildRequest,
) -> Result<crate::model::Model, EngineError> {
    let body: Value = serde_json::from_slice(&checkpoint.request.body)
        .map_err(|e| EngineError::bad(e.to_string()))?;
    let snapshot = config.lock().unwrap();
    let provider = snapshot
        .provider(&checkpoint.provider_id)
        .ok_or_else(|| EngineError::bad("fork provider unavailable"))?
        .with_caching(checkpoint.caching.clone());
    let model = crate::model::Model::new(
        checkpoint.provider_id.clone(),
        body["model"]
            .as_str()
            .ok_or_else(|| EngineError::bad("checkpoint model missing"))?
            .into(),
        provider,
        checkpoint.config.clone(),
        checkpoint.effort.as_deref(),
    )?;
    if model.command_latest() {
        return Err(EngineError::bad("history: latest disables hidden forks"));
    }
    crate::exclusive::check_model(&settings(), &model)?;
    Ok(model)
}
async fn converse(
    control: &Arc<Control>,
    config: &Arc<Mutex<code_config::ConfigStore>>,
    gate: &Arc<dyn crate::tools::PermissionGate>,
    model: &crate::model::Model,
    record: &mut Record,
    cancel: &CancellationToken,
) -> Result<(), EngineError> {
    let _slot = control
        .scheduler
        .short_named(&record.chat.asker, &record.chat.id, model, cancel)
        .await?;
    let weak = Arc::downgrade(control);
    let reader = ReadOnly::new(record.cwd.clone())
        .with_outputs(control.outputs_dir())
        .with_aliases(record.request.tool_aliases.clone())
        .with_allowed(
            record
                .request
                .catalog()
                .iter()
                .map(|d| record.request.canonical_tool_name(&d.name).to_owned())
                .collect(),
        )
        .with_web(crate::read_only::WebAccess {
            gate: gate.clone(),
            config: config.clone(),
            blocked: Arc::new(move || {
                weak.upgrade().is_some_and(|c| {
                    c.journal
                        .lock()
                        .unwrap()
                        .settings
                        .exclusive
                        .as_ref()
                        .is_some_and(|ex| ex.block_network)
                })
            }),
            agent_id: record.chat.asker.clone(),
            breadcrumb: format!("{} › fork chat", control.breadcrumb(&record.chat.asker)),
        });
    loop {
        crate::exclusive::check_model(&control.journal.lock().unwrap().settings, model)?;
        record
            .checkpoint
            .request
            .verify_fork_prefix(&record.request)?;
        let completion = control
            .auxiliary_unscheduled(model, &record.request, cancel)
            .await?;
        if !completion.status.executable() {
            return Err(EngineError::bad(
                "question fork received a non-executable reply",
            ));
        }
        if !completion.text.is_empty() {
            record.chat.messages.push(ForkChatMessage {
                tool_call: None,
                tool_result: None,
                role: "assistant".into(),
                text: completion.text.clone(),
                ts: sessions::now(),
            });
            let event = fork_event(record, "assistant", completion.text.clone());
            control.saved_chat_event(record, event)?;
        }
        let calls = completion.tool_calls.clone();
        let mut tail = vec![HistoryEvent::Assistant {
            text: completion.text,
            reasoning: completion.reasoning,
            tool_calls: calls.clone(),
            replay: completion.replay,
            status: completion.status,
            native_reason: completion.stop_reason,
        }];
        let proposed = Arc::new(Mutex::new(None));
        let captured = proposed.clone();
        let side = record.chat.question_id.is_empty();
        let mut functions = vec![];
        if !side {
            let question = control.question_by_call(&record.chat.question_id)?;
            let answer = ForkFunction {
                name: "answer_question".into(),
                description: ALLOWED.into(),
                call: Arc::new(move |_, args| {
                    if args["questionId"] != question.id {
                        return Err("answer_question must name this fork's questionId".into());
                    }
                    let answers: Vec<QuestionAnswer> =
                        serde_json::from_value(args["answers"].clone())
                            .map_err(|e| e.to_string())?;
                    crate::questions::validate(&question, &answers).map_err(|e| e.message)?;
                    let summary = args["summary"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 2000)
                        .ok_or("summary must be nonempty and at most 2,000 characters")?
                        .to_owned();
                    *captured.lock().unwrap() = Some((answers, summary));
                    Ok(json!({"answered":true}))
                }),
            };
            functions.push(answer);
        }
        if side {
            let weak = Arc::downgrade(control);
            let (asker, current) = (record.chat.asker.clone(), record.chat.id.clone());
            functions.push(ForkFunction {
                name: "btw_list".into(),
                description: "List earlier /btw side chats of this session and their exchanges"
                    .into(),
                call: Arc::new(move |_, _| {
                    let control = weak.upgrade().ok_or("session closed")?;
                    Ok(btw_list(&control, &asker, &current))
                }),
            });
            let weak = Arc::downgrade(control);
            let asker = record.chat.asker.clone();
            functions.push(ForkFunction {
                name: "btw_search".into(),
                description: "Search one /btw side chat: btw_search(chatId, query)".into(),
                call: Arc::new(move |_, args| {
                    let control = weak.upgrade().ok_or("session closed")?;
                    btw_search(&control, &asker, &args)
                }),
            });
        }
        let weak_history = Arc::downgrade(control);
        let asker = record.chat.asker.clone();
        functions.push(ForkFunction {
            name: "history".into(),
            description: "Read this session history, including side records".into(),
            call: Arc::new(move |_, mut args| {
                args["session"] = json!("current");
                let control = weak_history.upgrade().ok_or("session closed")?;
                let journal = control.journal.lock().unwrap();
                crate::session_tools::history(&journal, &asker, &args, None)
                    .map(|v| serde_json::from_str(&v).unwrap_or(Value::String(v)))
                    .map_err(|e| e.to_string())
            }),
        });
        for mut call in calls.iter().cloned() {
            let started = std::time::Instant::now();
            record.chat.messages.push(ForkChatMessage {
                role: "tool".into(),
                text: String::new(),
                ts: sessions::now(),
                tool_call: Some(call.clone()),
                tool_result: None,
            });
            control.saved_chat_event(
                record,
                EngineEvent::ForkChat {
                    fork_id: record.chat.id.clone(),
                    question_id: record.chat.question_id.clone(),
                    asker: record.chat.asker.clone(),
                    kind: "tool_start".into(),
                    text: String::new(),
                    messages: vec![],
                    tool_call: Some(call.clone()),
                    tool_result: None,
                },
            )?;
            let result = dispatch_call(
                &record.request,
                &reader,
                &functions,
                &mut call,
                (control, &record.chat.asker),
                cancel,
            )
            .await;
            let is_error = result.is_err();
            let output =
                result.unwrap_or_else(|e| format!("Error: {} {}", e.message, allowed(side)));
            let result = ToolResultData {
                output: output.clone(),
                is_error,
                details: ToolResultDetails {
                    duration_ms: Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64),
                    ..Default::default()
                },
            };
            let message = record.chat.messages.last_mut().unwrap();
            message.text = format!("{}: {output}", call.name);
            message.tool_call = Some(call.clone());
            message.tool_result = Some(result.clone());
            control.saved_chat_event(
                record,
                EngineEvent::ForkChat {
                    fork_id: record.chat.id.clone(),
                    question_id: record.chat.question_id.clone(),
                    asker: record.chat.asker.clone(),
                    kind: "tool".into(),
                    text: format!("{}: {output}", call.name),
                    messages: vec![],
                    tool_call: Some(call.clone()),
                    tool_result: Some(result.clone()),
                },
            )?;
            tail.push(HistoryEvent::ToolResult {
                details: result.details,
                id: call.id,
                name: call.name,
                output,
                is_error,
            });
        }
        record.request = record.request.with_history_tail(&tail)?;
        let answer = proposed.lock().unwrap().take();
        if let Some((answers, summary)) = answer {
            let envelope = {
                let mut j = control.journal.lock().unwrap();
                let question = j
                    .questions
                    .get(&record.chat.question_id)
                    .ok_or_else(|| EngineError::bad("question not found"))?;
                if question.answer.is_some() {
                    return Err(EngineError::bad(
                        "question was already answered outside this fork",
                    ));
                }
                crate::questions::validate(question, &answers)?;
                let (asker, id) = (question.asker.clone(), question.id.clone());
                j.append(
                    &asker,
                    HistoryEvent::QuestionForkAnswered {
                        id,
                        answers,
                        summary,
                    },
                )?
            };
            control.emit(EngineEvent::Committed { envelope });
            control.changed.notify_waiters();
            record.chat.closed = true;
        }
        control.save_chat(record)?;
        if record.chat.closed || calls.is_empty() {
            return Ok(());
        }
    }
}

/// Dispatch using the frozen request's canonical names while preserving wire calls
/// in the chat and provider replay.
async fn dispatch_call(
    request: &PreparedRequest,
    reader: &ReadOnly,
    functions: &[ForkFunction],
    call: &mut ToolCall,
    history: (&Control, &str),
    cancel: &CancellationToken,
) -> Result<String, EngineError> {
    let mut canonical = call.clone();
    canonical.name = request.canonical_tool_name(&call.name).to_owned();
    if !call.complete {
        Err(EngineError::bad("incomplete fork call"))
    } else if canonical.name == "script" {
        reader
            .fork_script(&canonical.arguments, 0, functions, cancel)
            .await
            .map(|(output, _)| output)
    } else if matches!(canonical.name.as_str(), "read" | "grep" | "glob") {
        reader.call(&canonical, cancel).await
    } else if crate::tools::web::handles(&canonical.name) {
        reader.web(&canonical, cancel).await
    } else if canonical.name == "history" {
        call.arguments["session"] = json!("current");
        crate::session_tools::history(
            &history.0.journal.lock().unwrap(),
            history.1,
            &call.arguments,
            None,
        )
        .map_err(|e| EngineError::bad(e.to_string()))
    } else {
        Err(EngineError::bad(format!(
            "{} is not available in this fork.",
            call.name
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session whose model is a command provider: it fails on "boom",
    /// takes a second on "slow" and otherwise answers "ok". Root has made
    /// one request, so it has a side chat.
    #[cfg(unix)]
    async fn side_chat_session() -> (tempfile::TempDir, Session) {
        let temp = tempfile::tempdir().unwrap();
        let session = Session::create(
            provider_store(temp.path(), "p"),
            temp.path().to_owned(),
            Default::default(),
        )
        .unwrap();
        let mut turn = session.send("hi");
        while let Some(event) = turn.recv().await {
            if matches!(event, EngineEvent::TurnEnd { .. }) {
                break;
            }
        }
        (temp, session)
    }

    /// A config under `root` whose only provider is `name` (the script).
    #[cfg(unix)]
    fn provider_store(root: &std::path::Path, name: &str) -> code_config::ConfigStore {
        let config = root.join(format!("config-{name}"));
        std::fs::create_dir_all(config.join("providers")).unwrap();
        std::fs::write(
            config.join("config.json"),
            json!({"defaultModel":format!("{name}/m"), "skillsDirs":[]}).to_string(),
        )
        .unwrap();
        let script = r#"if grep -q boom "$1"; then printf '%s\n' '{"type":"error","message":"boom","retryable":false}'; exit 1; fi; if grep -q slow "$1"; then sleep 1; fi; printf '%s\n' '{"type":"text","delta":"ok"}' '{"type":"done","stopReason":"end_turn"}'"#;
        std::fs::write(
            config.join(format!("providers/{name}.json")),
            json!({"type":"command", "command":{
                "argv":["sh", "-c", format!("cat > /dev/null; {script}"), "adapter", "{messagesFile}"],
                "output":"jsonl",
            }, "models":{"m":{"capabilities":{"input":["text"], "output":["text"], "tools":true}}}})
            .to_string(),
        )
        .unwrap();
        code_config::ConfigStore::load(code_config::Directories {
            config,
            data: root.join("data"),
            cache: root.join("cache"),
        })
        .unwrap()
    }

    /// The side chat's provider leaves the config (it stays valid: the
    /// default model moves to another provider).
    #[cfg(unix)]
    fn drop_provider(session: &Session, root: &std::path::Path) {
        *session.config.lock().unwrap() = provider_store(root, "q");
    }

    /// The fork chat's live events until it is idle; each one is checked
    /// against the saved record as it arrives: what it reports is already
    /// saved (a reopen's snapshot would have it).
    async fn until_idle(
        session: &Session,
        events: &mut tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
        fork: &str,
    ) -> Vec<(String, String)> {
        let mut seen = vec![];
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while let Some(event) = events.recv().await {
                let EngineEvent::ForkChat { kind, text, .. } = event else {
                    continue;
                };
                if matches!(kind.as_str(), "idle" | "closed") {
                    break;
                }
                if matches!(kind.as_str(), "user" | "assistant" | "error") {
                    let saved = session.question_fork(fork).unwrap().messages;
                    assert!(
                        saved.iter().any(|m| m.role == kind && m.text == text),
                        "{kind} {text} not saved before its event"
                    );
                }
                seen.push((kind, text));
            }
        })
        .await
        .expect("the side chat turn did not end");
        seen
    }

    fn roles(session: &Session, fork: &str) -> Vec<(String, String)> {
        session
            .question_fork(fork)
            .unwrap()
            .messages
            .into_iter()
            .map(|m| (m.role, m.text))
            .collect()
    }

    /// A side chat whose fork turn failed keeps the error in its record,
    /// so reopening the chat shows it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_side_chat_turn_saves_its_error() {
        let (temp, session) = side_chat_session().await;
        let mut events = session.subscribe();
        let fork = session.open_side_chat("root").unwrap();
        session.fork_chat_send(&fork, "boom").unwrap();
        let seen = until_idle(&session, &mut events, &fork).await;
        let error = seen
            .iter()
            .find(|(kind, _)| kind == "error")
            .map(|(_, text)| text.clone())
            .expect("no live error event");
        assert!(error.contains("boom"), "{error}");
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            roles(&session, &fork),
            [pair("user", "boom"), pair("error", &error)]
        );
        // Reopening hands the error back with the record.
        assert_eq!(session.open_side_chat("root").unwrap(), fork);
        let reopened = loop {
            match events.recv().await {
                Some(EngineEvent::ForkChat { kind, messages, .. }) if kind == "opened" => {
                    break messages;
                }
                Some(_) => {}
                None => panic!("events ended"),
            }
        };
        assert_eq!(reopened.last().map(|m| m.role.as_str()), Some("error"));
        // A send the fork cannot take at all (its provider is gone) is
        // saved as an error row too.
        drop_provider(&session, temp.path());
        let refused = session.fork_chat_send(&fork, "again").unwrap_err();
        assert_eq!(refused.message, "fork provider unavailable");
        let saved = roles(&session, &fork);
        assert_eq!(saved.len(), 3);
        assert_eq!(saved[2], pair("error", &refused.message));
        session.shutdown().await.unwrap();
    }

    /// A send refused while the fork is still answering leaves the record
    /// to the running reply: its answer is kept, nothing is added.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_send_refused_during_a_reply_leaves_the_record_alone() {
        let (temp, session) = side_chat_session().await;
        let mut events = session.subscribe();
        let fork = session.open_side_chat("root").unwrap();
        session.fork_chat_send(&fork, "slow").unwrap();
        drop_provider(&session, temp.path());
        let refused = session.fork_chat_send(&fork, "meanwhile").unwrap_err();
        assert_eq!(refused.message, "fork provider unavailable");
        // The reply (a second) is still running: the record is its own.
        assert_eq!(
            roles(&session, &fork),
            [("user".to_string(), "slow".to_string())]
        );
        let seen = until_idle(&session, &mut events, &fork).await;
        assert!(
            seen.iter()
                .any(|(kind, text)| kind == "assistant" && text == "ok")
        );
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            roles(&session, &fork),
            [pair("user", "slow"), pair("assistant", "ok")]
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn renamed_fork_tools_dispatch_without_changing_the_wire_name() {
        let (temp, store) = crate::test_support::store(json!({}));
        let session = Session::create(store, temp.path().to_owned(), Default::default()).unwrap();
        std::fs::write(temp.path().join("example.txt"), "needle\n").unwrap();
        session
            .control
            .commit(
                "root",
                HistoryEvent::SystemReminder {
                    text: "history".into(),
                },
            )
            .unwrap();
        let request: PreparedRequest = serde_json::from_value(json!({
            "prefix":"{}", "body":"{}", "wire":"command", "cache":null,
            "history_items":0, "offered_tools":[], "session_id":"session",
            "cache_key":"family", "conversation_id":"session:root", "agent_id":"root", "kind":"agent",
            "input_estimate":0, "retry_limit":0,
            "tool_aliases":{"read":"view_file", "grep":"find_text", "glob":"find_files", "script":"run_code", "history":"past_events", "web_search":"search_online", "web_fetch":"fetch_online"}
        })).unwrap();
        // Production readers get the session's canonical cwd, as here.
        let reader = ReadOnly::new(session.cwd()).with_aliases(request.tool_aliases.clone());
        for (name, arguments) in [
            ("view_file", json!({"path":"example.txt"})),
            (
                "find_text",
                json!({"path":"example.txt", "pattern":"needle"}),
            ),
            ("find_files", json!({"pattern":"*.txt"})),
            ("run_code", json!({"code":"print('script output')"})),
            (
                "past_events",
                json!({"action":"contexts", "session":"ignored-foreign-session"}),
            ),
        ] {
            let mut call = ToolCall {
                id: "call-1".into(),
                name: name.into(),
                arguments,
                complete: true,
            };
            let output = dispatch_call(
                &request,
                &reader,
                &[],
                &mut call,
                (&session.control, "root"),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(!output.is_empty(), "{name}");
            assert_eq!(call.name, name);
            if name == "past_events" {
                assert_eq!(call.arguments["session"], "current");
            }
        }
        for name in ["search_online", "fetch_online"] {
            let mut call = ToolCall {
                id: "call-1".into(),
                name: name.into(),
                arguments: json!({}),
                complete: true,
            };
            let error = dispatch_call(
                &request,
                &reader,
                &[],
                &mut call,
                (&session.control, "root"),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
            // The reader has no web access; reaching its error proves canonical dispatch.
            assert_eq!(error.message, crate::read_only::ALLOWED);
            assert_eq!(call.name, name);
        }
        session.shutdown().await.unwrap();
    }
}
