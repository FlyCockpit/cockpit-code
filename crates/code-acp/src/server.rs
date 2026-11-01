//! `cockpit-code acp`: the ACP v1 agent over stdio.

use crate::codec::{FrameError, LineReader};
use crate::rpc::{self, Out, RpcError, conforms, decode};
use crate::session::{AcpSession, Action, ClientCaps, parse_mode};
use agent_client_protocol_schema::v1;
use base64::Engine as _;
use code_config::{ConfigStore, Directories};
use code_engine::schema::{Attachment, ErrorKind};
use code_engine::{Session, SessionOptions, sessions};
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
/// arrives, then shut every session down (D38) before returning.
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
    let shutdowns = server.sessions.values().map(|s| s.session.shutdown());
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
                "sessionCapabilities": {"list": {}, "resume": {}, "close": {}},
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
        if let Ok(skills) = s.session.skills() {
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
            let req = decode::<v1::NewSessionRequest>(params)?;
            if !req.cwd.is_absolute() {
                return Err(RpcError::params("cwd must be an absolute path"));
            }
            if !req.mcp_servers.is_empty() {
                eprintln!(
                    "acp: ignoring {} client MCP server(s); cockpit-code uses its own mcps/ config",
                    req.mcp_servers.len()
                );
            }
            let store = self.store()?;
            let session = Session::create(
                store.clone(),
                req.cwd.clone(),
                SessionOptions {
                    model: self.options.model.clone(),
                    effort: self.options.effort.clone(),
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
            if replay {
                decode::<v1::LoadSessionRequest>(params)?;
            } else {
                decode::<v1::ResumeSessionRequest>(params)?;
            }
            if let Some(s) = self.sessions.get(&acp_id) {
                return Ok(s.clone());
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
            let session = opened.map_err(|e| match e.kind {
                ErrorKind::LockConflict => self.lock_error(&acp_id),
                _ if !self
                    .directories
                    .data
                    .join("sessions")
                    .join(&acp_id)
                    .is_dir() =>
                {
                    RpcError::new(rpc::NOT_FOUND, format!("no session {acp_id}"))
                }
                _ => RpcError::internal(e.message),
            })?;
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
                    crate::replay::replay(&self.out, &acp_id, &s.session);
                }
                let mut result = Self::settings_result(&s);
                result["_meta"] = json!({"cockpitCode": {
                    "engineSessionId": s.session.id(),
                    "forkedFrom": (s.session.id() != acp_id).then_some(&acp_id),
                }});
                self.out.respond(id, result);
                self.after_open(&s);
                s.restore_questions();
            }
        }
        None
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
            let (text, attachments) = prompt_content(&params["prompt"], &s.session)?;
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
                    _ => {
                        if let Some(command) = trimmed.strip_prefix('/') {
                            let (name, args) = command
                                .split_once(char::is_whitespace)
                                .unwrap_or((command, ""));
                            let explicit = name.starts_with("skill:");
                            let skill_name = name.strip_prefix("skill:").unwrap_or(name);
                            if explicit
                                || s.session
                                    .skills()
                                    .map_err(|e| RpcError::params(e.message))?
                                    .iter()
                                    .any(|skill| skill.name == skill_name)
                            {
                                Action::Send(
                                    s.session
                                        .skill_message(skill_name, args.trim())
                                        .map_err(|e| RpcError::params(e.message))?,
                                    attachments,
                                )
                            } else {
                                Action::Send(text, attachments)
                            }
                        } else {
                            Action::Send(text, attachments)
                        }
                    }
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
        let result = s.session.shutdown().await;
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
    session: &Session,
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
                    .attach_bytes(name, mime, &bytes)
                    .map_err(|e| RpcError::params(e.message))?,
            );
        }
    }
    Ok((text, attachments))
}
