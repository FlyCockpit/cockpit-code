//! One argv-only process per model request. No shell or provider-specific behavior.
use super::*;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Stdio};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const ARGV_LIMIT: usize = 64 * 1024;
const LINE_LIMIT: usize = 4 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RequestContext {
    pub cwd: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub parent_conversation_id: String,
    pub depth: usize,
    pub agent: String,
    pub permission_mode: String,
    pub latest: bool,
    pub output_file: Option<PathBuf>,
}

/// Only replayable conversation events. Metadata and credentials never enter model history.
pub(super) fn history(events: &[HistoryEvent]) -> Value {
    json!(
        events
            .iter()
            .filter_map(|e| match e {
                HistoryEvent::User { .. }
                | HistoryEvent::Assistant { .. }
                | HistoryEvent::SystemReminder { .. }
                | HistoryEvent::ToolResult { .. } => Some(e.clone()),
                HistoryEvent::ToolMedia { id, inputs } => Some(HistoryEvent::User {
                    text: format!("Media returned by tool call {id}:"),
                    attachments: inputs
                        .iter()
                        .filter_map(|m| match m {
                            MediaInput::Attachment { attachment } => Some(attachment.clone()),
                            _ => None,
                        })
                        .collect(),
                }),
                HistoryEvent::Compaction { summary } => Some(HistoryEvent::User {
                    text: summary.clone(),
                    attachments: vec![]
                }),
                _ => None,
            })
            .collect::<Vec<_>>()
    )
}

/// Single pass: substituted values are opaque, even when they contain placeholder syntax.
fn interpolate(template: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::new();
    let mut pos = 0;
    while pos < template.len() {
        let rest = &template[pos..];
        if rest.starts_with('{') {
            let mut matched = false;
            for (name, value) in vars {
                let token = format!("{{{name}}}");
                if rest.starts_with(&token) {
                    out.push_str(value);
                    pos += token.len();
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        pos += c.len_utf8();
    }
    if out.contains('\0') {
        return Err(EngineError::bad("command template contains a NUL byte"));
    }
    Ok(out)
}
fn argument(template: &str, vars: &BTreeMap<String, String>) -> Result<Option<String>> {
    if template.starts_with('{')
        && template.ends_with('}')
        && let Some((name, body)) = template[1..template.len() - 1].split_once('?')
    {
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return interpolate(template, vars).map(Some);
        }
        let value = vars
            .get(name)
            .ok_or_else(|| EngineError::bad("unknown conditional command variable"))?;
        return if value.is_empty() {
            Ok(None)
        } else {
            interpolate(body, vars).map(Some)
        };
    }
    interpolate(template, vars).map(Some)
}
fn io_error(_: impl std::fmt::Display) -> EngineError {
    EngineError::bad("command provider I/O failed")
}
fn hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

#[derive(Default, Serialize, Deserialize)]
struct Conversation {
    sent: usize,
    prefix_hash: String,
    provider_id: String,
}
struct Invocation {
    _dir: tempfile::TempDir,
    argv: Vec<String>,
    stdin: Option<String>,
    cwd: String,
    env: BTreeMap<String, String>,
    output: PathBuf,
    messages: Value,
    conversation: Conversation,
    state_file: Option<PathBuf>,
}
fn prepare(model: &Model, request: &PreparedRequest) -> Result<Invocation> {
    let config = model
        .provider
        .connection_command()
        .ok_or_else(|| EngineError::bad("missing command configuration"))?;
    let body: Value = serde_json::from_slice(&request.body).map_err(io_error)?;
    let messages = body["messages"].clone();
    let all = messages
        .as_array()
        .ok_or_else(|| EngineError::bad("invalid command history"))?;
    let dir = tempfile::Builder::new()
        .prefix("cockpit-code-provider-")
        .tempdir()
        .map_err(io_error)?;
    let mut vars = BTreeMap::<String, String>::new();
    let cwd = if request.command_context.cwd.as_os_str().is_empty() {
        std::env::current_dir().map_err(io_error)?
    } else {
        request.command_context.cwd.clone()
    };
    for (name, value) in [
        ("conversationId", request.conversation_identity().to_owned()),
        ("cacheKey", request.cache_identity().to_owned()),
        ("turnId", Uuid::new_v4().to_string()),
        (
            "parentConversationId",
            request.command_context.parent_conversation_id.clone(),
        ),
        ("depth", request.command_context.depth.to_string()),
        ("isSubagent", (request.agent_id != "root").to_string()),
        ("agent", request.command_context.agent.clone()),
        ("model", model.model_id.clone()),
        ("provider", model.provider_id.clone()),
        ("cwd", cwd.to_string_lossy().into_owned()),
        (
            "permissionMode",
            request.command_context.permission_mode.clone(),
        ),
        ("systemPrompt", body["system"].as_str().unwrap_or("").into()),
        ("messages_json", messages.to_string()),
        (
            "maxTokens",
            body["maxTokens"]
                .as_u64()
                .map(|n| n.to_string())
                .unwrap_or_default(),
        ),
        (
            "temperature",
            body["temperature"]
                .as_f64()
                .map(|n| n.to_string())
                .unwrap_or_default(),
        ),
        (
            "reasoningEffort",
            body["reasoningEffort"].as_str().unwrap_or("").into(),
        ),
    ] {
        vars.insert(name.into(), value);
    }
    let message = all
        .iter()
        .rev()
        .find_map(|m| match m["kind"].as_str() {
            Some("user") => m["text"].as_str(),
            Some("tool_result") => m["output"].as_str(),
            _ => None,
        })
        .unwrap_or("");
    vars.insert("message".into(), message.into());
    let transcript = all
        .iter()
        .map(|m| match m["kind"].as_str() {
            Some("user") => format!("user: {}", m["text"].as_str().unwrap_or("")),
            Some("assistant") => format!(
                "assistant: {}\n{}",
                m["text"].as_str().unwrap_or(""),
                if m["tool_calls"].as_array().is_some_and(|a| !a.is_empty()) {
                    m["tool_calls"].to_string()
                } else {
                    String::new()
                }
            ),
            Some("tool_result") => format!(
                "tool {}: {}",
                m["name"].as_str().unwrap_or(""),
                m["output"].as_str().unwrap_or("")
            ),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    vars.insert("transcript".into(), transcript);
    let state_file = if request.kind == "agent" {
        request.command_context.state_dir.as_ref().map(|root| {
            root.join(format!(
                "{:x}.json",
                Sha256::digest(
                    format!("{}:{}", request.conversation_identity(), model.id()).as_bytes()
                )
            ))
        })
    } else {
        None
    };
    let mut conversation: Conversation = state_file
        .as_ref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut since = 0;
    if model.command_latest()
        && conversation.sent <= all.len()
        && conversation.prefix_hash == hash(&json!(&all[..conversation.sent]))
    {
        since = conversation.sent;
        // The provider already owns the assistant it just produced; only results/new input are a delta.
        if all.get(since).is_some_and(|m| m["kind"] == "assistant") {
            since += 1;
        }
    } else if model.command_latest() {
        // A fork/restore/compaction changed the prefix. Start a new provider conversation.
        conversation = Conversation::default();
    }
    vars.insert(
        "providerConversationId".into(),
        conversation.provider_id.clone(),
    );
    let write = |name: &str, bytes: &[u8]| -> Result<String> {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).map_err(io_error)?;
        Ok(path.to_string_lossy().into_owned())
    };
    for (inline, file, name) in [
        ("message", "messageFile", "message.txt"),
        ("systemPrompt", "systemPromptFile", "system.txt"),
        ("messages_json", "messagesFile", "messages.json"),
        ("transcript", "transcriptFile", "transcript.txt"),
    ] {
        let path = write(name, vars[inline].as_bytes())?;
        vars.insert(file.into(), path);
    }
    vars.insert(
        "deltaFile".into(),
        write("delta.json", json!(&all[since..]).to_string().as_bytes())?,
    );
    vars.insert(
        "toolsFile".into(),
        write(
            "tools.json",
            serde_json::to_vec(&body["tools"])
                .map_err(io_error)?
                .as_slice(),
        )?,
    );
    let attachments = dir.path().join("attachments");
    std::fs::create_dir(&attachments).map_err(io_error)?;
    for message in all {
        for value in message["attachments"].as_array().into_iter().flatten() {
            let attachment: Attachment = serde_json::from_value(value.clone()).map_err(io_error)?;
            let bytes = crate::attachments::read(request.assets_dir.as_deref(), &attachment)?;
            std::fs::write(attachments.join(&attachment.path), bytes).map_err(io_error)?;
        }
    }
    vars.insert(
        "attachmentsDir".into(),
        attachments.to_string_lossy().into_owned(),
    );
    let output = request
        .command_context
        .output_file
        .clone()
        .unwrap_or_else(|| dir.path().join("output.txt"));
    vars.insert("outputFile".into(), output.to_string_lossy().into_owned());
    let render_argv = |vars: &BTreeMap<String, String>| -> Result<Vec<String>> {
        config
            .argv
            .iter()
            .filter_map(|arg| argument(arg, vars).transpose())
            .collect()
    };
    let mut argv = render_argv(&vars)?;
    if argv.iter().map(|s| s.len() + 1).sum::<usize>() > ARGV_LIMIT {
        let mut spilled = vars.clone();
        for (inline, file) in [
            ("message", "messageFile"),
            ("systemPrompt", "systemPromptFile"),
            ("messages_json", "messagesFile"),
            ("transcript", "transcriptFile"),
        ] {
            spilled.insert(inline.into(), format!("@{}", vars[file]));
        }
        argv = render_argv(&spilled)?;
        if argv.iter().map(|s| s.len() + 1).sum::<usize>() > ARGV_LIMIT {
            return Err(EngineError::bad(
                "command argv exceeds 64 KiB; use stdin or file placeholders",
            ));
        }
    }
    if argv.first().is_none_or(String::is_empty) {
        return Err(EngineError::bad("empty command argv"));
    }
    Ok(Invocation {
        argv,
        stdin: config
            .stdin
            .as_ref()
            .map(|s| interpolate(s, &vars))
            .transpose()?,
        cwd: interpolate(config.cwd.as_deref().unwrap_or("{cwd}"), &vars)?,
        env: config
            .env
            .iter()
            .map(|(k, v)| Ok((k.clone(), interpolate(v, &vars)?)))
            .collect::<Result<_>>()?,
        output,
        messages,
        conversation,
        state_file,
        _dir: dir,
    })
}

pub(crate) struct ProcessGroup(pub(crate) u32);
impl ProcessGroup {
    fn signal(&self, signal: i32) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0 as i32), signal);
        }
    }
    async fn stop(&self, child: &mut tokio::process::Child) {
        self.signal(libc::SIGINT);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let _ = child.try_wait();
            #[cfg(unix)]
            let alive = unsafe { libc::kill(-(self.0 as i32), 0) == 0 };
            #[cfg(not(unix))]
            let alive = false;
            if !alive || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.signal(libc::SIGKILL);
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}

fn text(state: &mut State, delta: &str, tx: &EventSender) {
    if delta.is_empty() {
        return;
    }
    state.semantic = true;
    state.activity += 1;
    state.text.push_str(delta);
    let _ = tx.send(EngineEvent::Text {
        delta: delta.into(),
    });
}

fn event(
    v: &Value,
    model: &Model,
    _request: &PreparedRequest,
    state: &mut State,
    tx: &EventSender,
    conversation: &mut Conversation,
) -> Result<()> {
    let string = |key: &str| {
        v[key]
            .as_str()
            .ok_or_else(|| EngineError::bad(format!("command event missing string {key}")))
    };
    if state.done {
        return Err(EngineError::bad("command event after done"));
    }
    match string("type")? {
        "start" => {
            if string("model")? != model.model_id {
                return Err(EngineError::bad(
                    "command start model differs from requested model",
                ));
            }
            if let Some(id) = v["providerConversationId"].as_str() {
                conversation.provider_id = id.into();
            }
        }
        "text" => text(state, string("delta")?, tx),
        "reasoning" => {
            if let Some(details) = v.get("details") {
                let details = details
                    .as_array()
                    .ok_or_else(|| EngineError::bad("reasoning details must be an array"))?;
                state.semantic |= !details.is_empty();
                state
                    .reasoning
                    .entry(0)
                    .or_default()
                    .details
                    .extend(details.clone());
            }
            state.reason(
                0,
                v.get("delta")
                    .map(|_| string("delta"))
                    .transpose()?
                    .unwrap_or(""),
                tx,
            );
        }
        "tool_call" => {
            let name = string("name")?;
            let id = string("id")?;
            if id.is_empty() || state.calls.values().any(|c| c.id == id) {
                return Err(EngineError::bad(
                    "command tool_call has an empty or duplicate id",
                ));
            }
            let index = state.calls.len();
            state.call(index, Some(id), Some(name), &v["arguments"].to_string(), tx);
            state.calls.get_mut(&index).unwrap().complete = true;
        }
        "usage" => {
            state.usage = v.clone();
            state.observe_cost(
                model
                    .config
                    .reported_cost
                    .as_ref()
                    .or(model.provider.reported_cost.as_ref()),
                v,
            );
        }
        "quota" => state.quota = Some(v.clone()),
        "error" => {
            let kind = match v["kind"].as_str() {
                Some("auth") => ErrorKind::Auth,
                Some("rate_limit") => ErrorKind::RateLimit,
                Some("context_overflow") => ErrorKind::ContextOverflow,
                Some("bad_request") => ErrorKind::BadRequest,
                _ => ErrorKind::Unavailable,
            };
            return Err(EngineError {
                kind,
                message: string("message")?.into(),
                retryable: v["retryable"].as_bool().unwrap_or(false),
                retry_after_secs: v["retryAfterSecs"].as_u64(),
            });
        }
        "done" => {
            let reason = string("stopReason")?;
            if !["end_turn", "tool_use", "max_tokens", "cancelled"].contains(&reason) {
                return Err(EngineError::bad("invalid command stopReason"));
            }
            if (reason == "tool_use") != !state.calls.is_empty()
                && reason != "max_tokens"
                && reason != "cancelled"
            {
                return Err(EngineError::bad("command done disagrees with tool calls"));
            }
            state.stop_reason = reason.into();
            state.done = true;
        }
        _ => return Err(EngineError::bad("unknown command protocol event")),
    }
    Ok(())
}

async fn read_stdout(
    mut stdout: impl AsyncRead + Unpin,
    model: &Model,
    request: &PreparedRequest,
    state: &mut State,
    tx: &EventSender,
    conversation: &mut Conversation,
) -> Result<()> {
    let mut pending = Vec::new();
    let mut buf = vec![0; 8192];
    let mut warned = false;
    loop {
        let n = stdout.read(&mut buf).await.map_err(io_error)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        if model.command_text() {
            let valid = match std::str::from_utf8(&pending) {
                Ok(s) => s.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => return Err(EngineError::bad("command stdout is not UTF-8")),
            };
            text(state, std::str::from_utf8(&pending[..valid]).unwrap(), tx);
            pending.drain(..valid);
        } else {
            while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                if end > LINE_LIMIT {
                    return Err(EngineError::bad("command JSONL line exceeds 4 MiB"));
                }
                let line: Vec<_> = pending.drain(..=end).collect();
                parse_line(&line, model, request, state, tx, conversation, &mut warned)?;
            }
            if pending.len() > LINE_LIMIT {
                return Err(EngineError::bad("command JSONL line exceeds 4 MiB"));
            }
        }
    }
    if !pending.is_empty() {
        if model.command_text() {
            return Err(EngineError::bad("command stdout ended in incomplete UTF-8"));
        }
        parse_line(
            &pending,
            model,
            request,
            state,
            tx,
            conversation,
            &mut warned,
        )?;
    }
    Ok(())
}
fn parse_line(
    line: &[u8],
    model: &Model,
    request: &PreparedRequest,
    state: &mut State,
    tx: &EventSender,
    conversation: &mut Conversation,
    warned: &mut bool,
) -> Result<()> {
    let line =
        std::str::from_utf8(line).map_err(|_| EngineError::bad("command JSONL is not UTF-8"))?;
    if line.trim().is_empty() {
        return Ok(());
    }
    if state.done {
        return Err(EngineError::bad("command output after done"));
    }
    match serde_json::from_str::<Value>(line) {
        Ok(v) => event(&v, model, request, state, tx, conversation),
        Err(_) => {
            if !*warned {
                *warned = true;
                let _ = tx.send(EngineEvent::Error {
                    error: EngineError::bad(
                        "command protocol warning: non-JSON stdout displayed as text",
                    ),
                });
            }
            text(state, line, tx);
            Ok(())
        }
    }
}

pub(super) async fn attempt(
    model: &Model,
    request: &PreparedRequest,
    tx: &EventSender,
    cancel: &CancellationToken,
    state: &mut State,
) -> Result<()> {
    if model.command_latest() && request.command_context.latest && request.kind != "agent" {
        return Err(EngineError::bad(
            "history: latest disables hidden forks and utility calls",
        ));
    }
    let mut invocation = prepare(model, request)?;
    let mut cmd = tokio::process::Command::new(&invocation.argv[0]);
    cmd.args(&invocation.argv[1..])
        .current_dir(&invocation.cwd)
        .envs(&invocation.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    if cancel.is_cancelled() {
        return Err(EngineError::cancelled());
    }
    let mut child = cmd
        .spawn()
        .map_err(|_| EngineError::bad("could not spawn command provider (check argv/cwd)"))?;
    let group = ProcessGroup(child.id().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut stderr_tail = Vec::new();
    let timeout = Duration::from_secs(
        model
            .provider
            .connection_command()
            .unwrap()
            .timeout_secs
            .unwrap_or(900),
    );
    let result = {
        let input = async {
            if let Some(text) = invocation.stdin.as_ref() {
                match stdin.write_all(text.as_bytes()).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                    Err(e) => return Err(io_error(e)),
                }
            }
            drop(stdin);
            Ok(())
        };
        let errors = async {
            let mut buf = vec![0; 8192];
            loop {
                let n = stderr.read(&mut buf).await.map_err(io_error)?;
                if n == 0 {
                    break;
                }
                stderr_tail.extend_from_slice(&buf[..n]);
                if stderr_tail.len() > STDERR_LIMIT {
                    stderr_tail.drain(..stderr_tail.len() - STDERR_LIMIT);
                }
            }
            Ok(())
        };
        let output = read_stdout(
            stdout,
            model,
            request,
            state,
            tx,
            &mut invocation.conversation,
        );
        let wait = async { child.wait().await.map_err(io_error) };
        let work = async {
            let (_, _, _, status) = tokio::try_join!(input, errors, output, wait)?;
            Ok(status)
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(EngineError::cancelled()),
            _ = tokio::time::sleep(timeout) => Err(EngineError::bad("command provider timeout")),
            result = work => result,
        }
    };
    let mut result = match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(EngineError {
            kind: ErrorKind::Unavailable,
            message: format!(
                "command provider exited {}",
                status
                    .code()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "by signal".into())
            ),
            retryable: status.code() == Some(75),
            retry_after_secs: None,
        }),
        Err(error) => {
            group.stop(&mut child).await;
            Err(error)
        }
    };
    if model.command_text() && result.is_ok() {
        // An unambiguous, versioned JSON trailer lives on one stderr line.
        for line in String::from_utf8_lossy(&stderr_tail).lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && v["protocol"] == code_config::discovery::PROTOCOL
                && v["type"] == "usage"
            {
                event(&v, model, request, state, tx, &mut invocation.conversation)?;
            }
        }
        if request.kind != "generate" && state.text.is_empty() && invocation.output.exists() {
            let bytes = tokio::fs::read(&invocation.output)
                .await
                .map_err(io_error)?;
            let content = std::str::from_utf8(&bytes).map_err(io_error)?;
            text(state, content, tx);
        }
        state.done = true;
        state.stop_reason = "end_turn".into();
    }
    if result.is_ok()
        && !model.command_text()
        && request.kind != "generate"
        && state.text.is_empty()
        && state.calls.is_empty()
        && invocation.output.exists()
    {
        let bytes = tokio::fs::read(&invocation.output)
            .await
            .map_err(io_error)?;
        let content = std::str::from_utf8(&bytes).map_err(io_error)?;
        text(state, content, tx);
    }
    if result.is_ok() && !state.done {
        result = Err(EngineError::bad(
            "command protocol error: missing done event",
        ));
    }
    if let Err(error) = &mut result
        && !cancel.is_cancelled()
        && !stderr_tail.is_empty()
    {
        let mut diagnostic = String::from_utf8_lossy(&stderr_tail).into_owned();
        for secret in invocation.env.values().filter(|s| s.len() >= 8) {
            diagnostic = diagnostic.replace(secret, "[redacted]");
        }
        error.message.push_str(&format!("\nstderr: {diagnostic}"));
    }
    if result.is_ok()
        && let Some(path) = invocation.state_file
    {
        invocation.conversation.sent = invocation.messages.as_array().unwrap().len();
        invocation.conversation.prefix_hash = hash(&invocation.messages);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(io_error)?;
        use std::io::Write;
        tmp.write_all(&serde_json::to_vec(&invocation.conversation).map_err(io_error)?)
            .map_err(io_error)?;
        tmp.persist(&path).map_err(io_error)?;
    }
    if model.command_latest() && result.is_ok() {
        let used = state.usage["contextUsed"].as_u64();
        let window = state.usage["contextWindow"].as_u64();
        let _ = tx.send(EngineEvent::Context {
            meter: ContextMeter {
                used: used.unwrap_or(0),
                window,
                output_reservation: model.config.max_output_tokens.unwrap_or(8192),
                source: if used.is_some() && window.is_some() {
                    "provider"
                } else {
                    "unknown"
                }
                .into(),
                warning: used
                    .zip(window)
                    .is_some_and(|(u, w)| u >= w.saturating_mul(9) / 10),
            },
        });
    }
    result
}
