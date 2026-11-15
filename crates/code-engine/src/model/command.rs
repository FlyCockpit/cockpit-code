//! One argv-only process per model request. No shell or provider-specific behavior.
use super::*;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Stdio};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const ARGV_LIMIT: usize = 64 * 1024;
const LINE_LIMIT: usize = 4 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
/// All JSONL stdout of one invocation (text, reasoning, details, tool calls
/// are retained in memory): twice the assistant-text cap.
const JSONL_TOTAL_LIMIT: u64 = 2 * crate::spill::TEXT_CAP;

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
/// A text `outputFile` becomes the assistant message, which has to live in
/// memory and in the session journal, so unlike generated media it cannot
/// spill to a file: more than `cap` bytes is refused instead of read.
async fn read_text_output(path: &std::path::Path, cap: u64) -> Result<String> {
    // Nonblocking, so a FIFO left at the path cannot hang the turn.
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .await
        .map_err(io_error)?;
    if !file.metadata().await.map_err(io_error)?.is_file() {
        return Err(EngineError::bad(
            "command provider outputFile is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(io_error)?;
    if bytes.len() as u64 > cap {
        return Err(EngineError::bad(format!(
            "command provider outputFile exceeds {} MiB of text",
            cap >> 20
        )));
    }
    String::from_utf8(bytes).map_err(io_error)
}
fn hash(value: &Value) -> String {
    crate::hex::hex(&Sha256::digest(value.to_string().as_bytes()))
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
    if request.conversation_id.trim().is_empty() {
        return Err(EngineError::bad("conversation_id must be nonempty"));
    }
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
                "{}.json",
                crate::hex::hex(&Sha256::digest(
                    format!("{}:{}", request.conversation_identity(), model.id()).as_bytes()
                ))
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
    pub(crate) fn new(pgid: u32) -> Self {
        crate::process_groups::track(pgid as i32);
        Self(pgid)
    }
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
        crate::process_groups::forget(self.0 as i32);
    }
}

/// Assistant text from a command provider (stdout, JSONL `text` events or
/// the outputFile) is held in memory, so it is capped like the outputFile.
fn text(state: &mut State, delta: &str, tx: &EventSender) -> Result<()> {
    if delta.is_empty() {
        return Ok(());
    }
    if (state.text.len() + delta.len()) as u64 > crate::spill::TEXT_CAP {
        return Err(EngineError::bad(format!(
            "command provider text exceeds {} MiB",
            crate::spill::TEXT_CAP >> 20
        )));
    }
    state.semantic = true;
    state.activity += 1;
    state.text.push_str(delta);
    let _ = tx.send(EngineEvent::Text {
        delta: delta.into(),
    });
    Ok(())
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
        "text" => text(state, string("delta")?, tx)?,
        "reasoning" => {
            if let Some(details) = v.get("details") {
                let details = details
                    .as_array()
                    .ok_or_else(|| EngineError::bad("reasoning details must be an array"))?;
                state.details_cost += details.iter().map(crate::spill::value_cost).sum::<u64>();
                if state.details_cost > crate::spill::TEXT_CAP {
                    return Err(EngineError::bad(format!(
                        "command provider reasoning details exceed {} MiB",
                        crate::spill::TEXT_CAP >> 20
                    )));
                }
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
    let mut total = 0u64;
    let mut scanned = 0;
    loop {
        let n = stdout.read(&mut buf).await.map_err(io_error)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if !model.command_text() && total > JSONL_TOTAL_LIMIT {
            return Err(EngineError::bad(format!(
                "command provider JSONL output exceeds {} MiB",
                JSONL_TOTAL_LIMIT >> 20
            )));
        }
        pending.extend_from_slice(&buf[..n]);
        if model.command_text() {
            let valid = match std::str::from_utf8(&pending) {
                Ok(s) => s.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => return Err(EngineError::bad("command stdout is not UTF-8")),
            };
            text(state, std::str::from_utf8(&pending[..valid]).unwrap(), tx)?;
            pending.drain(..valid);
        } else {
            // Only bytes not yet searched are scanned for the newline, so a
            // long line costs linear time rather than a rescan per read.
            while let Some(end) = pending[scanned..]
                .iter()
                .position(|b| *b == b'\n')
                .map(|i| i + scanned)
            {
                if end > LINE_LIMIT {
                    return Err(EngineError::bad("command JSONL line exceeds 4 MiB"));
                }
                let line: Vec<_> = pending.drain(..=end).collect();
                scanned = 0;
                parse_line(&line, model, request, state, tx, conversation, &mut warned)?;
            }
            scanned = pending.len();
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
            text(state, line, tx)
        }
    }
}

fn diagnostic_secrets(
    configured: impl IntoIterator<Item = String>,
    inherited: impl IntoIterator<Item = (String, String)>,
) -> Vec<String> {
    let mut secrets: Vec<_> =
        configured
            .into_iter()
            .chain(inherited.into_iter().filter_map(|(name, value)| {
                code_config::is_credential_name(&name).then_some(value)
            }))
            .filter(|value| value.len() >= 8)
            .collect();
    // Replace longer overlapping secrets first so a shorter value cannot expose
    // the suffix of a longer one.
    secrets.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    secrets
}

fn redact_diagnostic(mut diagnostic: String, secrets: &[String]) -> String {
    for secret in secrets {
        diagnostic = diagnostic.replace(secret, "[redacted]");
    }
    diagnostic
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
    let secrets = diagnostic_secrets(
        model
            .provider
            .connection_command()
            .unwrap()
            .redaction_values
            .iter()
            .chain(invocation.env.values())
            .cloned(),
        std::env::vars_os().filter_map(|(name, value)| {
            Some((name.into_string().ok()?, value.into_string().ok()?))
        }),
    );
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
    let group = ProcessGroup::new(child.id().unwrap());
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
            let content = read_text_output(&invocation.output, crate::spill::TEXT_CAP).await?;
            text(state, &content, tx)?;
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
        let content = read_text_output(&invocation.output, crate::spill::TEXT_CAP).await?;
        text(state, &content, tx)?;
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
        let diagnostic = String::from_utf8_lossy(&stderr_tail);
        error.message.push_str(&format!("\nstderr: {diagnostic}"));
    }
    if let Err(error) = &mut result {
        error.message = redact_diagnostic(std::mem::take(&mut error.message), &secrets);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulated_command_text_is_capped() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = State::default();
        let chunk = "x".repeat(1 << 20);
        for _ in 0..crate::spill::TEXT_CAP >> 20 {
            text(&mut state, &chunk, &tx).unwrap();
        }
        assert_eq!(
            text(&mut state, "y", &tx).unwrap_err().message,
            "command provider text exceeds 64 MiB"
        );
        assert_eq!(state.text.len() as u64, crate::spill::TEXT_CAP);
    }

    #[tokio::test]
    async fn text_output_file_is_read_up_to_the_text_cap_and_refused_above_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.txt");
        let cap = 1 << 20;
        let text = "é".repeat(cap as usize / 2);
        std::fs::write(&path, &text).unwrap();
        assert_eq!(read_text_output(&path, cap).await.unwrap(), text);
        std::fs::write(&path, format!("{text}x")).unwrap();
        let error = read_text_output(&path, cap).await.unwrap_err();
        assert_eq!(
            error.message,
            "command provider outputFile exceeds 1 MiB of text"
        );
        std::fs::write(&path, b"\xff").unwrap();
        assert_eq!(
            read_text_output(&path, cap).await.unwrap_err().message,
            "command provider I/O failed"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_errors_preserve_noncredential_expansions_and_redact_explicit_env() {
        let expanded = std::env::var("PATH").unwrap();
        assert!(expanded.len() >= 8);
        for (script, output) in [
            (
                r#"read -r input; printf '%s %s %s' "$1" "$input" "$EXPLICIT" >&2; exit 1"#,
                "text",
            ),
            (
                r#"printf '{"type":"error","message":"expanded=%s explicit=%s"}\n' "$1" "$EXPLICIT""#,
                "jsonl",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("providers")).unwrap();
            let provider = json!({"type":"command", "command":{
                "argv":["sh", "-c", script, "adapter", "${PATH}"],
                "stdin":"${PATH}\n", "output":output,
                "env":{"EXPLICIT":"explicit-secret-value"}
            }, "models":{"m":{}}});
            std::fs::write(root.path().join("providers/p.json"), provider.to_string()).unwrap();
            let store = code_config::ConfigStore::load(code_config::Directories {
                config: root.path().into(),
                data: root.path().into(),
                cache: root.path().join("cache"),
            })
            .unwrap();
            assert!(store.snapshot().errors.is_empty());
            let model = Model {
                client: crate::http_client(Client::builder).unwrap(),
                provider: store.snapshot().providers["p"].clone(),
                config: store.snapshot().providers["p"].models["m"].clone(),
                provider_id: "p".into(),
                model_id: "m".into(),
                effort: None,
            };
            let mut request: PreparedRequest =
                serde_json::from_value(super::super::tests::request_value()).unwrap();
            request.body = serde_json::to_vec(&json!({"messages":[]})).unwrap();
            request.command_context.cwd = root.path().into();
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let error = attempt(
                &model,
                &request,
                &tx,
                &CancellationToken::new(),
                &mut State::default(),
            )
            .await
            .unwrap_err();
            assert!(error.message.contains(&expanded));
            assert!(!error.message.contains("explicit-secret-value"));
            assert!(error.message.contains("[redacted]"));
            if output == "text" {
                assert!(error.message.contains("stderr:"));
            }
        }
    }

    /// Runs a JSONL command provider whose stdout comes from `script`
    /// (stdin is drained first) and returns its error.
    async fn jsonl_script_error(script: impl FnOnce(&std::path::Path) -> String) -> EngineError {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("providers")).unwrap();
        let script = format!("cat > /dev/null; {}", script(root.path()));
        let provider = json!({"type":"command", "command":{
            "argv":["sh", "-c", script], "output":"jsonl",
        }, "models":{"m":{}}});
        std::fs::write(root.path().join("providers/p.json"), provider.to_string()).unwrap();
        let store = code_config::ConfigStore::load(code_config::Directories {
            config: root.path().into(),
            data: root.path().into(),
            cache: root.path().join("cache"),
        })
        .unwrap();
        let model = Model {
            client: crate::http_client(Client::builder).unwrap(),
            provider: store.snapshot().providers["p"].clone(),
            config: store.snapshot().providers["p"].models["m"].clone(),
            provider_id: "p".into(),
            model_id: "m".into(),
            effort: None,
        };
        let mut request: PreparedRequest =
            serde_json::from_value(super::super::tests::request_value()).unwrap();
        request.body = serde_json::to_vec(&json!({"messages":[]})).unwrap();
        request.command_context.cwd = root.path().into();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        attempt(
            &model,
            &request,
            &tx,
            &CancellationToken::new(),
            &mut State::default(),
        )
        .await
        .unwrap_err()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn jsonl_stdout_is_refused_past_the_total_limit() {
        // Every JSONL byte counts; blank 1 MiB lines keep the test fast. They
        // are written to a file by the shell, as argv is capped at 64 KiB.
        let error = jsonl_script_error(|root| {
            let line = root.join("line.jsonl");
            format!(
                r#"{{ head -c 1048575 /dev/zero | tr '\0' ' '; echo; }} > '{line}'; while cat '{line}'; do :; done | head -c {limit}"#,
                line = line.display(),
                limit = JSONL_TOTAL_LIMIT + (4 << 20)
            )
        })
        .await;
        assert!(
            error
                .message
                .starts_with("command provider JSONL output exceeds 128 MiB"),
            "{}",
            error.message
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retained_reasoning_details_are_budgeted() {
        // ~2 MB lines of one million-node `details` arrays: far under the
        // wire limits, but each retains ~64 MB of JSON values.
        let error = jsonl_script_error(|root| {
            let line = root.join("details.jsonl");
            format!(
                r#"{{ printf '{{"type":"reasoning","details":['; yes 0, | head -n 999999 | tr -d '\n'; printf '0]}}\n'; }} > '{line}'; cat '{line}' '{line}' '{line}'"#,
                line = line.display(),
            )
        })
        .await;
        assert!(
            error
                .message
                .starts_with("command provider reasoning details exceed 64 MiB"),
            "{}",
            error.message
        );
    }

    #[test]
    fn diagnostics_redact_expanded_values_and_inherited_credentials() {
        let mut inherited = Vec::new();
        for (index, name) in [
            "OPENAI_API_KEY",
            "token",
            "client_SeCrEt",
            "PASSWORD",
            "PASSWD",
            "CREDENTIAL",
            "APIKEY",
            "CREDENTIALS",
            "AUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "GITHUB_TOKEN",
        ]
        .iter()
        .enumerate()
        {
            inherited.push((name.to_string(), format!("inherited-secret-{index}")));
        }
        inherited.extend([
            ("PATH".into(), "ordinary-value".into()),
            ("SHORT_KEY".into(), "short".into()),
            ("HOME".into(), "/home/alice".into()),
            ("GIT_AUTHOR_NAME".into(), "Alice Example".into()),
            ("SSH_AUTH_SOCK".into(), "/tmp/auth-socket".into()),
            ("XAUTHORITY".into(), "/home/alice/.Xauthority".into()),
            ("AUTHORIZATION".into(), "ordinary-header".into()),
        ]);
        let secrets = diagnostic_secrets(
            [
                "expanded-value".into(),
                "explicit-env-value".into(),
                "short".into(),
            ],
            inherited.clone(),
        );
        let diagnostic = format!(
            "expanded-value explicit-env-value {}",
            inherited
                .iter()
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let redacted = redact_diagnostic(diagnostic, &secrets);
        assert_eq!(redacted.matches("[redacted]").count(), 13);
        assert!(redacted.ends_with(
            "ordinary-value short /home/alice Alice Example /tmp/auth-socket /home/alice/.Xauthority ordinary-header"
        ));
        assert!(!redacted.contains("inherited-secret"));
    }

    #[test]
    fn diagnostic_redaction_handles_overlapping_values() {
        let secrets = diagnostic_secrets(["abcdefgh".into(), "abcdefgh-more".into()], []);
        assert_eq!(
            redact_diagnostic("abcdefgh-more abcdefgh".into(), &secrets),
            "[redacted] [redacted]"
        );
    }

    #[test]
    fn empty_conversation_identity_is_rejected_before_command_preparation() {
        let provider = serde_json::from_value(json!({"type":"command"})).unwrap();
        let config = serde_json::from_value(json!({})).unwrap();
        let model = Model {
            client: crate::http_client(Client::builder).unwrap(),
            provider,
            config,
            provider_id: "p".into(),
            model_id: "m".into(),
            effort: None,
        };
        let mut request: PreparedRequest =
            serde_json::from_value(super::super::tests::request_value()).unwrap();
        request.conversation_id.clear();
        let error = prepare(&model, &request).err().unwrap();
        assert_eq!(error.message, "conversation_id must be nonempty");
    }
}
