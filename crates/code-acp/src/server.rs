//! `cockpit-code acp`: the ACP v1 agent over stdio.

use crate::codec::{FrameError, LineReader};
use crate::rpc::{self, Out, RpcError, conforms, decode};
use crate::session::{AcpSession, Action, ClientCaps, parse_mode};
use agent_client_protocol_schema::v1;
use base64::Engine as _;
use code_config::{ConfigStore, Directories};
use code_engine::context::{ClientPrompt, ClientPromptMode};
use code_engine::schema::{Attachment, ErrorKind};
use code_engine::{Session, SessionOptions, sessions};
use serde::Deserialize as _;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

#[derive(Clone, Default)]
pub struct Options {
    /// Use flat subagents for newly created sessions.
    pub flat: bool,
    /// Model and effort for new sessions.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Permission mode for new sessions (`manual|auto|unattended|yolo`).
    pub mode: Option<String>,
}

const PAGE: usize = 100;

/// Serve ACP on stdin/stdout until the client disconnects or a signal
/// arrives, then shut every session down before returning.
pub async fn run(options: Options) -> Result<(), String> {
    let directories = Directories::resolve().map_err(|e| e.to_string())?;
    let usage_store = ConfigStore::load(directories.clone()).map_err(|e| e.to_string())?;
    let _usage = code_engine::usage::Cache::start(&usage_store);
    let (tx, mut frames) = mpsc::unbounded_channel::<String>();
    let out = Arc::new(Out::new(tx));
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        // An empty frame is the shutdown sentinel; real frames are never empty.
        while let Some(frame) = frames.recv().await {
            if frame.is_empty() {
                break;
            }
            let mut line = frame.into_bytes();
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let (in_tx, mut inbound) = mpsc::unbounded_channel::<Result<String, FrameError>>();
    std::thread::spawn(move || {
        let mut reader = LineReader::new(std::io::stdin().lock());
        loop {
            let frame = reader.read_frame();
            let end = matches!(
                frame,
                Ok(None) | Err(FrameError::IncompleteEof { .. } | FrameError::Io(_))
            );
            let frame = match frame {
                Ok(None) => break,
                Ok(Some(f)) => Ok(f),
                Err(e) => Err(e),
            };
            if in_tx.send(frame).is_err() || end {
                break;
            }
        }
    });
    let mut server = Server {
        out: out.clone(),
        directories,
        options,
        caps: ClientCaps::default(),
        sessions: HashMap::new(),
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    loop {
        tokio::select! {
            frame = inbound.recv() => match frame {
                None => break,
                Some(Ok(json)) => server.frame(&json).await,
                Some(Err(FrameError::Empty)) => {}
                Some(Err(e)) => {
                    eprintln!("acp: {e}");
                    out.fail(&Value::Null, RpcError::new(-32700, e.to_string()));
                }
            },
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    // Disconnect: unanswered approvals become denials, open questions stay
    // pending on disk, and every session tree stops and releases its lock.
    out.close();
    let shutdowns = server.sessions.values().map(|s| s.shutdown());
    for result in futures_util::future::join_all(shutdowns).await {
        if let Err(e) = result {
            eprintln!("acp: shutdown: {}", e.message);
        }
    }
    let _ = out_sentinel(&out);
    let _ = writer.await;
    Ok(())
}

fn out_sentinel(out: &Out) -> Result<(), ()> {
    out.raw_send(String::new())
}

struct Server {
    out: Arc<Out>,
    directories: Directories,
    options: Options,
    caps: ClientCaps,
    sessions: HashMap<String, Arc<AcpSession>>,
}

impl Server {
    async fn frame(&mut self, json: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(json) else {
            self.out
                .fail(&Value::Null, RpcError::new(-32700, "invalid JSON"));
            return;
        };
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            if msg.get("result").is_some() || msg.get("error").is_some() {
                self.out.on_response(&msg);
            } else {
                self.out.fail(
                    &id.unwrap_or(Value::Null),
                    RpcError::new(-32600, "not a JSON-RPC request, notification or response"),
                );
            }
            return;
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let Some(id) = id else {
            self.notification(method, &params);
            return;
        };
        let reply = self.request(&id, method, &params).await;
        if let Some(reply) = reply {
            self.out.reply(&id, reply);
        }
    }

    fn notification(&mut self, method: &str, params: &Value) {
        if method == "session/cancel"
            && let Ok(n) = decode::<v1::CancelNotification>(params)
            && let Some(s) = self.sessions.get(n.session_id.0.as_ref())
        {
            s.cancel();
        }
    }

    /// `None` when the response is sent later (prompts) or already sent.
    async fn request(&mut self, id: &Value, method: &str, params: &Value) -> Option<rpc::Reply> {
        Some(match method {
            "initialize" => self.initialize(params),
            "authenticate" => Ok(json!({})),
            "session/new" => return self.new_session(id, params),
            "session/load" => return self.load(id, params, true),
            "session/resume" => return self.load(id, params, false),
            "session/list" => self.list(params),
            "session/prompt" => return self.prompt(id, params),
            "session/set_mode" => self.set_mode(params),
            "session/set_config_option" => self.set_config(params).await,
            "session/close" => self.close(params).await,
            _ => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )),
        })
    }

    fn session(&self, params: &Value) -> Result<Arc<AcpSession>, RpcError> {
        let id = params["sessionId"].as_str().unwrap_or("");
        self.sessions
            .get(id)
            .cloned()
            .ok_or_else(|| RpcError::new(rpc::NOT_FOUND, format!("unknown session {id}")))
    }

    fn initialize(&mut self, params: &Value) -> rpc::Reply {
        decode::<v1::InitializeRequest>(params)?;
        self.caps.elicitation_form = !params["clientCapabilities"]["elicitation"]["form"].is_null();
        let result = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": true, "audio": true, "embeddedContext": true},
                "mcpCapabilities": {"http": false, "sse": false},
                "sessionCapabilities": {
                    "list": {}, "resume": {}, "close": {},
                    "systemPrompt": {"mode": {"override": {}}},
                },
            },
            "agentInfo": {
                "name": "cockpit-code", "title": "cockpit-code",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "authMethods": [],
        });
        conforms::<v1::InitializeResponse>("initialize", &result);
        Ok(result)
    }

    fn store(&self) -> Result<ConfigStore, RpcError> {
        ConfigStore::load(self.directories.clone()).map_err(|e| RpcError::internal(e.to_string()))
    }

    fn settings_result(s: &AcpSession) -> Value {
        json!({"modes": s.modes(), "configOptions": s.config_options()})
    }

    fn after_open(&self, s: &AcpSession) {
        let mut commands = vec![
            json!({"name":"subagents","description":"Choose tree/flat before the first message","input":{"hint":"tree | flat"}}),
            json!({"name":"decisions","description":"Review decisions made on your behalf","input":{"hint":"list | search text | keep id | change id steering | discuss id question"}}),
            json!({"name":"guidance","description":"View injected guidance and selection reasons"}),
            json!({"name":"prune","description":"Prune finished prompts/old outputs; deliberately breaks the prompt cache"}),
            json!({"name":"compact","description":"Summarize into a fresh context","input":{"hint":"optional focus"}}),
            json!({"name":"goal","description":"Draft/run criteria; pause, resume, clear or status","input":{"hint":"objective | pause | resume | clear"}}),
        ];
        if let Ok(skills) = s.skills() {
            for skill in skills {
                let shadowed = [
                    "compact",
                    "prune",
                    "goal",
                    "guidance",
                    "decisions",
                    "subagents",
                ]
                .contains(&skill.name.as_str());
                let name = if shadowed {
                    format!("skill:{}", skill.name)
                } else {
                    skill.name.clone()
                };
                let description = format!(
                    "{}{}{}{}",
                    skill.description,
                    if skill.disable_model_invocation {
                        " [you only]"
                    } else {
                        ""
                    },
                    if skill.shadows_global {
                        " [project overrides global]"
                    } else {
                        ""
                    },
                    if shadowed {
                        " [builtin wins; use skill: alias]"
                    } else {
                        ""
                    }
                );
                commands.push(json!({"name":name,"description":description,"input":{"hint":"optional arguments"}}));
            }
        }
        self.out.update(
            &s.acp_id,
            json!({"sessionUpdate":"available_commands_update","availableCommands":commands}),
        );
    }

    fn new_session(&mut self, id: &Value, params: &Value) -> Option<rpc::Reply> {
        let result = (|| {
            let client_mcp_count = params["mcpServers"].as_array().map_or(0, Vec::len);
            let system_prompt = client_prompt(params)?;
            let req = decode::<v1::NewSessionRequest>(&normalize_session_params(params))?;
            if !req.cwd.is_absolute() {
                return Err(RpcError::params("cwd must be an absolute path"));
            }
            if client_mcp_count != 0 {
                eprintln!(
                    "acp: ignoring {} client MCP server(s); cockpit-code uses its own mcps/ config",
                    client_mcp_count
                );
            }
            let store = self.store()?;
            let session = Session::create(
                store.clone(),
                req.cwd.clone(),
                SessionOptions {
                    model: self.options.model.clone(),
                    effort: self.options.effort.clone(),
                    system_prompt,
                },
            )
            .map_err(|e| RpcError::internal(e.message))?;
            if self.options.flat {
                session
                    .set_subagents(code_engine::SubagentMode::Flat)
                    .map_err(|e| RpcError::params(e.message))?;
            }
            let mode = self.options.mode.as_deref().and_then(parse_mode);
            let sid = session.id();
            let s = AcpSession::start(
                sid.clone(),
                session,
                store,
                self.out.clone(),
                self.caps,
                false,
                mode,
            );
            let mut result = Self::settings_result(&s);
            result["sessionId"] = json!(sid);
            conforms::<v1::NewSessionResponse>("session/new", &result);
            self.sessions.insert(sid, s);
            Ok(result)
        })();
        let opened = result
            .as_ref()
            .ok()
            .and_then(|r| r["sessionId"].as_str())
            .map(String::from);
        self.out.reply(id, result);
        if let Some(s) = opened.and_then(|sid| self.sessions.get(&sid)) {
            self.after_open(s);
        }
        None
    }

    /// `session/load` (with history replay) and `session/resume` (without).
    /// A held lock fails with `SESSION_LOCKED`; `_meta.cockpitCode.lock`
    /// set to `"fork"` or `"claim"` opens a copy or takes the lock over.
    fn load(&mut self, id: &Value, params: &Value, replay: bool) -> Option<rpc::Reply> {
        let acp_id = params["sessionId"].as_str().unwrap_or("").to_string();
        let lock = params["_meta"]["cockpitCode"]["lock"]
            .as_str()
            .map(String::from);
        let result = (|| {
            let system_prompt = client_prompt(params)?;
            if replay {
                decode::<v1::LoadSessionRequest>(&normalize_session_params(params))?;
            } else {
                decode::<v1::ResumeSessionRequest>(&normalize_session_params(params))?;
            }
            if let Some(s) = self.sessions.get(&acp_id) {
                same_system_prompt(&acp_id, system_prompt.as_ref(), s.system_prompt())?;
                return Ok(s.clone());
            }
            if let Some(supplied) = &system_prompt {
                let stored = sessions::stored_system_prompt(&self.directories.data, &acp_id)
                    .map_err(|e| self.open_error(&acp_id, e))?;
                same_system_prompt(&acp_id, Some(supplied), stored)?;
            }
            let store = self.store()?;
            let options = SessionOptions::default();
            let opened = match lock.as_deref() {
                None => Session::resume(store.clone(), &acp_id, options),
                Some("claim") => Session::claim(store.clone(), &acp_id, options),
                Some("fork") => Session::fork(store.clone(), &acp_id, None, options),
                Some(other) => {
                    return Err(RpcError::params(format!(
                        "_meta.cockpitCode.lock must be \"fork\" or \"claim\", not {other:?}"
                    )));
                }
            };
            let session = opened.map_err(|e| self.open_error(&acp_id, e))?;
            let s = AcpSession::start(
                acp_id.clone(),
                session,
                store,
                self.out.clone(),
                self.caps,
                true,
                None,
            );
            self.sessions.insert(acp_id.clone(), s.clone());
            Ok(s)
        })();
        match result {
            Err(e) => self.out.fail(id, e),
            Ok(s) => {
                if replay {
                    s.replay();
                }
                let mut result = Self::settings_result(&s);
                result["_meta"] = s.open_meta();
                self.out.respond(id, result);
                self.after_open(&s);
                s.restore_questions();
            }
        }
        None
    }

    fn open_error(&self, id: &str, e: code_engine::schema::EngineError) -> RpcError {
        match e.kind {
            ErrorKind::LockConflict => self.lock_error(id),
            _ if !self.directories.data.join("sessions").join(id).is_dir() => {
                RpcError::new(rpc::NOT_FOUND, format!("no session {id}"))
            }
            _ => RpcError::internal(e.message),
        }
    }

    fn lock_error(&self, id: &str) -> RpcError {
        let status = sessions::lock_status(&self.directories.data, id).ok();
        let holder = status.as_ref().and_then(|s| s.holder.clone());
        let alive = status.as_ref().and_then(|s| s.pid_alive);
        let who = holder
            .as_ref()
            .map(|h| {
                format!(
                    "pid {} on {} since {}{}",
                    h.pid,
                    h.host,
                    h.started_at,
                    match alive {
                        Some(true) => " (running)",
                        Some(false) => " (not running: safe to claim)",
                        None => "",
                    }
                )
            })
            .unwrap_or_else(|| "another process".into());
        let mut e = RpcError::new(
            rpc::SESSION_LOCKED,
            format!(
                "session {id} is locked by {who}. Load it again with _meta {{\"cockpitCode\":{{\"lock\":\"fork\"}}}} to open a copy, or {{\"cockpitCode\":{{\"lock\":\"claim\"}}}} to take it over (the other process becomes read-only)."
            ),
        );
        e.data = Some(json!({
            "sessionId": id,
            "holder": holder.map(|h| json!({"pid": h.pid, "host": h.host, "startedAt": h.started_at})),
            "pidAlive": alive,
            "options": [
                {"lock": "fork", "description": "open a copy of the session with no lock conflict"},
                {"lock": "claim", "description": "take over the lock; the holder becomes read-only"},
            ],
        }));
        e
    }

    fn list(&self, params: &Value) -> rpc::Reply {
        let req = decode::<v1::ListSessionsRequest>(params)?;
        let rows = sessions::list_in(&self.directories.data, req.cwd.as_deref().map(Path::new))
            .map_err(|e| RpcError::internal(e.message))?;
        let rows: Vec<_> = rows.into_iter().filter(|r| !r.title.is_empty()).collect();
        let start = req
            .cursor
            .as_deref()
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or(0);
        let sessions: Vec<Value> = rows
            .iter()
            .skip(start)
            .take(PAGE)
            .map(|r| {
                json!({
                    "sessionId": r.header.id, "cwd": r.header.cwd, "title": r.title,
                    "updatedAt": r.updated_at,
                    "_meta": {"cockpitCode": {
                        "locked": r.lock.holder.is_some(), "model": r.model, "cost": r.cost,
                    }},
                })
            })
            .collect();
        let mut result = json!({"sessions": sessions});
        if start + PAGE < rows.len() {
            result["nextCursor"] = json!((start + PAGE).to_string());
        }
        conforms::<v1::ListSessionsResponse>("session/list", &result);
        Ok(result)
    }

    fn prompt(&mut self, id: &Value, params: &Value) -> Option<rpc::Reply> {
        let result = (|| {
            decode::<v1::PromptRequest>(params)?;
            let s = self.session(params)?;
            let (text, attachments) = prompt_content(&params["prompt"], &s)?;
            let trimmed = text.trim();
            let action = if attachments.is_empty()
                && trimmed
                    .strip_prefix("/subagents")
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            {
                Action::Subagents(trimmed[10..].trim().to_owned())
            } else if attachments.is_empty()
                && trimmed
                    .strip_prefix("/decisions")
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            {
                Action::Decisions(trimmed[10..].trim().to_owned())
            } else if attachments.is_empty() && trimmed == "/guidance" {
                Action::Guidance
            } else if attachments.is_empty() && trimmed.starts_with("/guidance ") {
                return Err(RpcError::params("/guidance takes no arguments"));
            } else if attachments.is_empty() && trimmed == "/prune" {
                Action::Prune
            } else if attachments.is_empty() && trimmed.starts_with("/prune ") {
                return Err(RpcError::params("/prune takes no arguments"));
            } else if attachments.is_empty()
                && trimmed
                    .strip_prefix("/goal")
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            {
                Action::Goal(trimmed[5..].trim().to_owned())
            } else {
                match trimmed.strip_prefix("/compact") {
                    Some(rest)
                        if attachments.is_empty()
                            && (rest.is_empty() || rest.starts_with(char::is_whitespace)) =>
                    {
                        let focus = rest.trim();
                        Action::Compact((!focus.is_empty()).then(|| focus.to_owned()))
                    }
                    _ => match trimmed.strip_prefix('/') {
                        Some(command) => match s
                            .skill_prompt(command)
                            .map_err(|e| RpcError::params(e.message))?
                        {
                            Some(message) => Action::Send(message, attachments),
                            None => Action::Send(text, attachments),
                        },
                        None => Action::Send(text, attachments),
                    },
                }
            };
            s.prompt(id.clone(), action);
            Ok(())
        })();
        result.err().map(Err)
    }

    fn set_mode(&mut self, params: &Value) -> rpc::Reply {
        let req = decode::<v1::SetSessionModeRequest>(params)?;
        let s = self.session(params)?;
        let mode = parse_mode(&req.mode_id.0).ok_or_else(|| RpcError::params("unknown mode"))?;
        s.set_mode(mode)?;
        s.announce_settings();
        Ok(json!({}))
    }

    async fn set_config(&mut self, params: &Value) -> rpc::Reply {
        decode::<v1::SetSessionConfigOptionRequest>(params)?;
        let s = self.session(params)?;
        let config = params["configId"].as_str().unwrap_or("");
        let value = params["value"]
            .as_str()
            .ok_or_else(|| RpcError::params("value must be a string"))?;
        s.set_config_ready(config, value).await?;
        s.announce_settings();
        Ok(json!({"configOptions": s.config_options()}))
    }

    async fn close(&mut self, params: &Value) -> rpc::Reply {
        let id = params["sessionId"].as_str().unwrap_or("").to_string();
        let s = self.session(params)?;
        s.prepare_close();
        let result = s.shutdown().await;
        self.sessions.remove(&id);
        s.stop();
        result.map_err(|e| RpcError::internal(e.message))?;
        Ok(json!({}))
    }
}

/// Text portions of ACP content; binary content is imported separately.
fn prompt_text(blocks: &Value) -> String {
    let mut out = String::new();
    for b in blocks.as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => out.push_str(b["text"].as_str().unwrap_or("")),
            Some("resource_link") => {
                let uri = b["uri"].as_str().unwrap_or("");
                out.push_str(&format!(
                    " @{} ",
                    uri.strip_prefix("file://").unwrap_or(uri)
                ));
            }
            Some("resource") => {
                let r = &b["resource"];
                let uri = r["uri"].as_str().unwrap_or("");
                match r["text"].as_str() {
                    Some(text) => {
                        out.push_str(&format!("\n<context uri=\"{uri}\">\n{text}\n</context>\n"))
                    }
                    None => out.push_str(&format!(" @{uri} ")),
                }
            }
            Some("image" | "audio") => {}
            Some(other) => {
                out.push_str(&format!("\n[{other}]\n"));
            }
            None => {}
        }
    }
    out
}

fn prompt_content(
    blocks: &Value,
    session: &AcpSession,
) -> Result<(String, Vec<Attachment>), RpcError> {
    let text = prompt_text(blocks);
    let mut attachments = Vec::new();
    for block in blocks.as_array().into_iter().flatten() {
        let (resource, data) = match block["type"].as_str() {
            Some("image" | "audio") => (block, block["data"].as_str()),
            Some("resource") => (&block["resource"], block["resource"]["blob"].as_str()),
            _ => continue,
        };
        if let Some(data) = data {
            if data.len() as u64 > code_engine::attachments::MAX_BYTES * 4 / 3 + 4 {
                return Err(RpcError::params("attachment exceeds 20 MiB"));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| RpcError::params("attachment data is not base64"))?;
            let mime = resource["mimeType"]
                .as_str()
                .unwrap_or("application/octet-stream");
            let name = resource["uri"]
                .as_str()
                .and_then(|uri| uri.rsplit('/').next())
                .filter(|s| !s.is_empty())
                .unwrap_or("attachment");
            attachments.push(
                session
                    .attach(name, mime, &bytes)
                    .map_err(|e| RpcError::params(e.message))?,
            );
        }
    }
    Ok((text, attachments))
}

/// A request's `systemPrompt` as the engine stores it; `None` when absent or
/// null. ACP v1 requires an error, before any session opens, for anything the
/// agent can't apply: an unadvertised mode, a non-text block, empty content.
/// The value is decoded on its own, before the request, so neither a schema
/// error nor any other reply echoes the text; it is never logged.
fn client_prompt(params: &Value) -> Result<Option<ClientPrompt>, RpcError> {
    let Some(value) = params.get("systemPrompt").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let prompt = v1::SystemPrompt::deserialize(value).map_err(|_| {
        RpcError::params(
            "systemPrompt must be {\"mode\": \"append\" | \"override\", \"content\": [{\"type\": \"text\", \"text\": ...}]}",
        )
    })?;
    let mode = match prompt.mode {
        v1::SystemPromptMode::Append => ClientPromptMode::Append,
        v1::SystemPromptMode::Override => ClientPromptMode::Override,
        _ => {
            return Err(RpcError::params(
                "unsupported systemPrompt mode: cockpit-code supports \"append\" and \"override\"",
            ));
        }
    };
    if prompt.content.is_empty() {
        return Err(RpcError::params("systemPrompt.content must not be empty"));
    }
    let mut texts = Vec::with_capacity(prompt.content.len());
    for block in &prompt.content {
        match block {
            v1::ContentBlock::Text(t) => texts.push(t.text.as_str()),
            _ => {
                return Err(RpcError::params(
                    "systemPrompt.content supports only text blocks",
                ));
            }
        }
    }
    ClientPrompt::new(mode, texts.join("\n\n"))
        .map(Some)
        .map_err(|e| RpcError::params(format!("systemPrompt: {}", e.message)))
}

/// A session's system prompt is fixed when it is created (and stored with it),
/// so load and resume may omit it or repeat it, but not change it.
fn same_system_prompt(
    id: &str,
    supplied: Option<&ClientPrompt>,
    stored: Option<ClientPrompt>,
) -> Result<(), RpcError> {
    match supplied {
        Some(supplied) if stored.as_ref() != Some(supplied) => Err(RpcError::params(format!(
            "{}; a session keeps the system prompt it was created with. Send the same value, or omit systemPrompt to keep the stored one.",
            if stored.is_some() {
                format!("systemPrompt differs from the one session {id} was created with")
            } else {
                format!("session {id} was created without a systemPrompt")
            }
        ))),
        _ => Ok(()),
    }
}

/// Client MCP servers and extra directories are ignored by this server. Clear
/// present values before schema decoding, including entries the schema cannot
/// represent, while preserving required-field checks for absent keys.
fn normalize_session_params(params: &Value) -> Value {
    let mut params = params.clone();
    if let Some(object) = params.as_object_mut() {
        for key in ["mcpServers", "additionalDirectories"] {
            if let Some(value) = object.get_mut(key) {
                *value = json!([]);
            }
        }
    }
    params
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1;
    use serde_json::{Value, json};

    use std::time::Duration;

    struct Harness {
        root: std::path::PathBuf,
        server: super::Server,
        frames: tokio::sync::mpsc::UnboundedReceiver<String>,
        next_id: u64,
    }
    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    impl Harness {
        /// A server whose only model `p/m` is a command provider that appends
        /// each request's agent role and frozen system prompt to `requests.log`.
        fn new(name: &str, provider: bool) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "code-acp-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let config = root.join("config");
            if provider {
                std::fs::create_dir_all(config.join("providers")).unwrap();
                std::fs::write(config.join("config.json"), r#"{"skillsDirs":[]}"#).unwrap();
                let log = root.join("requests.log");
                std::fs::write(config.join("providers/p.json"), json!({
                    "type":"command", "command":{
                        "argv":["sh", "-c", r#"printf '<<<request agent=%s>>>\n' "$1" >> "$3"; cat "$2" >> "$3"; printf '\n<<<end>>>\n' >> "$3"; printf '%s\n' '{"type":"text","delta":"ok"}' '{"type":"done","stopReason":"end_turn"}'"#, "fake", "{agent}", "{systemPromptFile}", log],
                        "output":"jsonl",
                    },
                    "models":{"m":{"capabilities":{"output":["text"]}}},
                }).to_string()).unwrap();
            }
            let (tx, frames) = tokio::sync::mpsc::unbounded_channel();
            let server = super::Server {
                out: std::sync::Arc::new(super::Out::new(tx)),
                directories: code_config::Directories {
                    config,
                    data: root.join("data"),
                    cache: root.join("cache"),
                },
                options: super::Options {
                    model: provider.then(|| "p/m".into()),
                    ..Default::default()
                },
                caps: Default::default(),
                sessions: Default::default(),
            };
            Self {
                root,
                server,
                frames,
                next_id: 0,
            }
        }

        /// Sends one request and returns its response, skipping notifications.
        async fn call(&mut self, method: &str, params: Value) -> Value {
            self.next_id += 1;
            let id = self.next_id;
            let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            self.server.frame(&request.to_string()).await;
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let frame: Value =
                        serde_json::from_str(&self.frames.recv().await.unwrap()).unwrap();
                    if frame["id"] == id && frame.get("method").is_none() {
                        return frame;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{method} got no response"))
        }

        async fn ok(&mut self, method: &str, params: Value) -> Value {
            let reply = self.call(method, params).await;
            assert!(reply.get("error").is_none(), "{method}: {reply}");
            reply["result"].clone()
        }

        async fn refused(&mut self, method: &str, params: Value) -> String {
            let reply = self.call(method, params).await;
            assert_eq!(reply["error"]["code"], -32602, "{method}: {reply}");
            reply["error"]["message"].as_str().unwrap().to_owned()
        }

        /// One turn, which must send exactly one new build-agent model request.
        async fn prompt(&mut self, sid: &str) {
            let before = self.build_requests().len();
            let result = self
                .ok(
                    "session/prompt",
                    json!({"sessionId": sid, "prompt": [{"type": "text", "text": "hello"}]}),
                )
                .await;
            assert_eq!(result["stopReason"], "end_turn", "{result}");
            assert_eq!(
                self.build_requests().len(),
                before + 1,
                "one new model request"
            );
        }

        /// System prompts of the build-agent requests logged so far.
        fn build_requests(&self) -> Vec<String> {
            let log = std::fs::read_to_string(self.root.join("requests.log")).unwrap_or_default();
            log.split("<<<request agent=")
                .filter_map(|r| r.strip_prefix("build>>>\n"))
                .map(|r| r.split("\n<<<end>>>").next().unwrap().to_owned())
                .collect()
        }

        fn cwd(&self) -> Value {
            std::fs::create_dir_all(self.root.join("work")).unwrap();
            json!(self.root.join("work"))
        }
    }

    fn text_prompt(mode: &str, text: &str) -> Value {
        json!({"mode": mode, "content": [{"type": "text", "text": text}]})
    }

    #[tokio::test]
    async fn initialize_advertises_append_and_override_system_prompts() {
        let mut h = Harness::new("system-prompt-caps", false);
        let result = h
            .ok(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": {}}),
            )
            .await;
        let response: v1::InitializeResponse = serde_json::from_value(result).unwrap();
        let caps = response
            .agent_capabilities
            .session_capabilities
            .system_prompt;
        assert!(
            caps.and_then(|c| c.mode)
                .and_then(|m| m.override_)
                .is_some()
        );
    }

    #[tokio::test]
    async fn unusable_system_prompts_are_refused_before_any_session_opens() {
        let mut h = Harness::new("system-prompt-refused", false);
        let cwd = h.cwd();
        let text = |t: &str| json!([{"type": "text", "text": t}]);
        let cases = [
            (
                json!({"mode": "someFutureMode", "content": text("x")}),
                "unsupported systemPrompt mode",
            ),
            (
                json!({"mode": "_vendor", "content": text("x")}),
                "unsupported systemPrompt mode",
            ),
            (
                json!({"mode": "append", "content": []}),
                "must not be empty",
            ),
            (
                json!({"mode": "override", "content": text(" \n ")}),
                "text is empty",
            ),
            (
                json!({"mode": "append", "content": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}]}),
                "only text blocks",
            ),
            (
                json!({"mode": "append", "content": [{"type": "resource_link", "uri": "file:///etc/passwd", "name": "passwd"}]}),
                "only text blocks",
            ),
            (
                json!({"mode": "append", "content": text(&"x".repeat(code_engine::context::CLIENT_PROMPT_MAX_BYTES + 1))}),
                "the limit is",
            ),
        ];
        for (prompt, expected) in cases {
            for (method, mut params) in [
                ("session/new", json!({"cwd": cwd, "mcpServers": []})),
                (
                    "session/load",
                    json!({"sessionId": "s", "cwd": cwd, "mcpServers": []}),
                ),
                ("session/resume", json!({"sessionId": "s", "cwd": cwd})),
            ] {
                params["systemPrompt"] = prompt.clone();
                let message = h.refused(method, params).await;
                assert!(message.contains(expected), "{method}: {message}");
                assert!(!message.contains("xxxx"), "the text is never echoed");
            }
        }
        // A wrongly typed value fails schema decoding, with the same code and a
        // fixed message that never echoes the value.
        for bad in [
            json!("SECRET-WRONG-TYPE"),
            json!({"content": text("SECRET-NO-MODE")}),
            json!({"mode": "append"}),
            json!({"mode": "append", "content": [{"type": "text", "text": 7}]}),
            json!({"mode": "append", "content": [{"type": "SECRET-TYPE", "text": "x"}]}),
            json!({"mode": "append", "content": "SECRET-NOT-A-LIST"}),
        ] {
            for (method, mut params) in [
                ("session/new", json!({"cwd": cwd, "mcpServers": []})),
                (
                    "session/load",
                    json!({"sessionId": "s", "cwd": cwd, "mcpServers": []}),
                ),
                ("session/resume", json!({"sessionId": "s", "cwd": cwd})),
            ] {
                params["systemPrompt"] = bad.clone();
                let message = h.refused(method, params).await;
                assert!(
                    message.starts_with("systemPrompt must be"),
                    "{method}: {message}"
                );
                assert!(!message.contains("SECRET"), "{method}: {message}");
            }
        }
        assert!(h.server.sessions.is_empty());
        assert!(
            !h.root.join("data").exists(),
            "no session storage was touched"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn append_prompt_reaches_every_turn_and_survives_load_and_resume() {
        let mut h = Harness::new("system-prompt-append", true);
        let cwd = h.cwd();
        // Literal text: no `${ENV}`, `{placeholder}` or `@file` expansion.
        let marker = "CLIENT-APPEND-7f3a: answer in French. ${HOME} {cwd} @{systemPromptFile}";
        let created = h
            .ok(
                "session/new",
                json!({"cwd": cwd, "mcpServers": [], "systemPrompt": {"mode": "append", "content": [
                    {"type": "text", "text": "CLIENT-APPEND-7f3a: answer in French."},
                    {"type": "text", "text": "${HOME} {cwd} @{systemPromptFile}"},
                ]}}),
            )
            .await;
        let sid = created["sessionId"].as_str().unwrap().to_owned();
        let marker = marker.replacen(". $", ".\n\n$", 1);
        h.prompt(&sid).await;
        h.prompt(&sid).await;
        let requests = h.build_requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        for system in &requests {
            // Appended after cockpit-code's own instructions, which stay.
            let at = system
                .find(&marker)
                .expect("client text in the system prompt");
            assert!(system[..at].contains("You are a coding agent"), "{system}");
            assert!(system[..at].contains("Agent role instructions"), "{system}");
        }
        // Reopening keeps the stored prompt, whether the client repeats it or omits it.
        let same = json!({"mode": "append", "content": [
            {"type": "text", "text": "CLIENT-APPEND-7f3a: answer in French."},
            {"type": "text", "text": "${HOME} {cwd} @{systemPromptFile}"},
        ]});
        for (method, prompt) in [
            ("session/resume", same.clone()),
            ("session/load", Value::Null),
            ("session/resume", Value::Null),
            ("session/load", same.clone()),
        ] {
            h.ok("session/close", json!({"sessionId": sid})).await;
            assert!(h.server.sessions.is_empty());
            h.ok(
                method,
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": [], "systemPrompt": prompt}),
            )
            .await;
            h.prompt(&sid).await;
            let last = h.build_requests().pop().unwrap();
            assert_eq!(last, requests[0], "{method}: the same frozen prompt");
        }
        // A different prompt is refused, for the open session and a stored one.
        let other = text_prompt("append", "something else");
        for open in [true, false] {
            if !open {
                h.ok("session/close", json!({"sessionId": sid})).await;
            }
            for method in ["session/load", "session/resume"] {
                let message = h
                    .refused(
                        method,
                        json!({"sessionId": sid, "cwd": cwd, "mcpServers": [], "systemPrompt": other}),
                    )
                    .await;
                assert!(
                    message.contains("differs from the one session"),
                    "{message}"
                );
            }
            let message = h
                .refused(
                    "session/load",
                    json!({"sessionId": sid, "cwd": cwd, "mcpServers": [],
                           "systemPrompt": {"mode": "override", "content": same["content"]}}),
                )
                .await;
            assert!(
                message.contains("differs"),
                "mode is part of the prompt: {message}"
            );
        }
        assert!(h.server.sessions.is_empty(), "a refused load opens nothing");
        // Forks keep the source's prompt.
        let forked = h
            .call(
                "session/load",
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": [], "systemPrompt": same,
                       "_meta": {"cockpitCode": {"lock": "fork"}}}),
            )
            .await;
        assert!(forked.get("error").is_none(), "{forked}");
        // The copy is served under the requested id; its own id is in _meta.
        let fork_id = forked["result"]["_meta"]["cockpitCode"]["engineSessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(fork_id, sid);
        let stored = super::client_prompt(&json!({"systemPrompt": same})).unwrap();
        assert_eq!(h.server.sessions[&sid].system_prompt(), stored);
        h.prompt(&sid).await;
        assert_eq!(
            h.build_requests().pop().unwrap(),
            requests[0],
            "the fork's model request"
        );
        // ... and after the fork itself is reopened.
        h.ok("session/close", json!({"sessionId": sid})).await;
        assert_eq!(
            code_engine::sessions::stored_system_prompt(&h.server.directories.data, &fork_id)
                .unwrap(),
            stored
        );
        h.ok("session/resume", json!({"sessionId": fork_id, "cwd": cwd}))
            .await;
        h.prompt(&fork_id).await;
        assert_eq!(
            h.build_requests().pop().unwrap(),
            requests[0],
            "the reopened fork"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn override_prompt_replaces_the_base_instructions_only() {
        let mut h = Harness::new("system-prompt-override", true);
        let cwd = h.cwd();
        let created = h
            .ok(
                "session/new",
                json!({"cwd": cwd, "mcpServers": [],
                       "systemPrompt": text_prompt("override", "CLIENT-OVERRIDE-91c2: you are a reviewer.")}),
            )
            .await;
        let sid = created["sessionId"].as_str().unwrap().to_owned();
        h.prompt(&sid).await;
        let system = h.build_requests().pop().unwrap();
        assert!(
            system.starts_with("CLIENT-OVERRIDE-91c2: you are a reviewer.\n"),
            "{system}"
        );
        assert!(!system.contains("You are a coding agent"), "{system}");
        // Harness rules, tools and the role stay; the override text appears once.
        assert!(system.contains("<system-message id="), "{system}");
        assert!(system.contains("Agent role instructions"), "{system}");
        assert_eq!(system.matches("CLIENT-OVERRIDE-91c2").count(), 1);
        h.ok("session/close", json!({"sessionId": sid})).await;
        h.ok("session/resume", json!({"sessionId": sid, "cwd": cwd}))
            .await;
        h.prompt(&sid).await;
        assert_eq!(h.build_requests().pop().unwrap(), system);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sessions_without_a_system_prompt_use_the_defaults_and_refuse_a_late_one() {
        let mut h = Harness::new("system-prompt-none", true);
        let cwd = h.cwd();
        let created = h
            .ok(
                "session/new",
                json!({"cwd": cwd, "mcpServers": [], "systemPrompt": null}),
            )
            .await;
        let sid = created["sessionId"].as_str().unwrap().to_owned();
        h.prompt(&sid).await;
        let system = h.build_requests().pop().unwrap();
        assert!(system.starts_with("You are a coding agent"), "{system}");
        assert!(
            !system.contains("Session instructions from the client"),
            "{system}"
        );
        h.ok("session/close", json!({"sessionId": sid})).await;
        let message = h
            .refused(
                "session/resume",
                json!({"sessionId": sid, "cwd": cwd, "systemPrompt": text_prompt("append", "late")}),
            )
            .await;
        assert!(
            message.contains("was created without a systemPrompt"),
            "{message}"
        );
        // An unknown session is still "not found", with or without a prompt.
        let reply = h
            .call(
                "session/resume",
                json!({"sessionId": "00000000-0000-4000-8000-000000000000", "cwd": cwd,
                       "systemPrompt": text_prompt("append", "x")}),
            )
            .await;
        assert_eq!(reply["error"]["code"], super::rpc::NOT_FOUND, "{reply}");
    }

    macro_rules! accepts {
        ($ty:ty, $value:expr) => {
            let _ = serde_json::from_value::<$ty>($value).unwrap();
        };
    }

    #[test]
    fn schema_ignores_unsupported_client_mcp_entries() {
        let params = json!({
            "sessionId": "s", "cwd": "/workspace",
            "mcpServers": [
                {"type": "http", "name": "http", "url": "https://example.com"},
                {"type": "sse", "name": "sse", "url": "https://example.com"},
                {"name": "stdio", "command": "server", "args": [], "env": []},
                {"type": "acp", "name": "acp"}
            ],
            "additionalDirectories": ["/extra", {"invalid": true}]
        });
        // The strict schema rejects these entries; the server owns ignoring them.
        assert!(super::decode::<v1::NewSessionRequest>(&params).is_err());
        let normalized = super::normalize_session_params(&params);
        let request = super::decode::<v1::NewSessionRequest>(&normalized).unwrap();
        assert!(request.mcp_servers.is_empty());
        accepts!(v1::LoadSessionRequest, normalized.clone());
        accepts!(v1::ResumeSessionRequest, normalized.clone());
        assert_eq!(normalized["additionalDirectories"], json!([]));
        assert_eq!(params["mcpServers"].as_array().unwrap().len(), 4);
        assert_eq!(params["additionalDirectories"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn session_normalization_preserves_required_fields_and_other_validation() {
        let params = json!({"sessionId": "s", "cwd": "/workspace"});
        let normalized = super::normalize_session_params(&params);
        assert_eq!(normalized, params);
        assert!(super::decode::<v1::NewSessionRequest>(&normalized).is_err());
        assert!(super::decode::<v1::LoadSessionRequest>(&normalized).is_err());
        accepts!(v1::ResumeSessionRequest, normalized);
        for ignored in [json!(null), json!({"invalid": true}), json!(42)] {
            let normalized = super::normalize_session_params(&json!({
                "cwd": "/workspace", "mcpServers": ignored,
                "additionalDirectories": ignored
            }));
            accepts!(v1::NewSessionRequest, normalized);
        }
        let normalized = super::normalize_session_params(&json!({"mcpServers": []}));
        assert!(super::decode::<v1::NewSessionRequest>(&normalized).is_err());
        assert_eq!(super::normalize_session_params(&json!(null)), json!(null));
    }

    #[test]
    fn schema_accepts_existing_v1_requests_without_new_optional_fields() {
        accepts!(
            v1::InitializeRequest,
            json!({"protocolVersion": 1, "clientCapabilities": {}})
        );
        accepts!(
            v1::NewSessionRequest,
            json!({"cwd": "/workspace", "mcpServers": []})
        );
        accepts!(
            v1::LoadSessionRequest,
            json!({"sessionId": "s", "cwd": "/workspace", "mcpServers": []})
        );
        accepts!(
            v1::ResumeSessionRequest,
            json!({"sessionId": "s", "cwd": "/workspace"})
        );
        accepts!(
            v1::ListSessionsRequest,
            json!({"cursor": "", "cwd": "/workspace"})
        );
        accepts!(
            v1::PromptRequest,
            json!({"sessionId": "s", "prompt": [{"type": "text", "text": "hello"}]})
        );
        accepts!(
            v1::SetSessionModeRequest,
            json!({"sessionId": "s", "modeId": "manual"})
        );
        accepts!(
            v1::SetSessionConfigOptionRequest,
            json!({"sessionId": "s", "configId": "model", "value": "test"})
        );
        accepts!(v1::CancelNotification, json!({"sessionId": "s"}));
    }

    #[test]
    fn schema_accepts_existing_v1_responses_notifications_and_permission_forms() {
        accepts!(
            v1::InitializeResponse,
            json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true}, "authMethods": []})
        );
        accepts!(v1::NewSessionResponse, json!({"sessionId": "s"}));
        accepts!(
            v1::ListSessionsResponse,
            json!({"sessions": [{"sessionId": "s", "cwd": "/workspace", "title": "test"}], "nextCursor": "100"})
        );
        accepts!(
            v1::SessionNotification,
            json!({"sessionId": "s", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hello"}}})
        );
        accepts!(
            v1::RequestPermissionRequest,
            json!({"sessionId": "s", "toolCall": {"toolCallId": "t", "status": "pending", "content": []}, "options": [{"optionId": "once", "name": "Allow once", "kind": "allow_once"}]})
        );
        accepts!(
            v1::CreateElicitationRequest,
            json!({"mode": "form", "sessionId": "s", "message": "question", "requestedSchema": {"type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"]}})
        );
    }
}
