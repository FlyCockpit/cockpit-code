//! Media generation: `generate {kind, prompt, model?, inputs?, outPath?}`.
//!
//! Offered only when an enabled model lists image, video or audio output. Media
//! models are never chat-loop models here (`Model::new` requires text output) and
//! never subagent models (`code_config::subagent_eligible`), so this tool speaks to
//! them directly with one non-streaming request.
//!
//! Wires:
//! - `openai-responses`: `POST {baseUrl}/responses`; reads `image_generation_call`
//!   items (`result` is base64 or a `data:` URL; OpenRouter returns the latter).
//! - `openai-chat`: `POST {baseUrl}/chat/completions`; reads `message.images[]`
//!   (`image_url.url` data URLs) and `message.audio.data`.
//! - Request params follow the model's `imageRequest`: `modalities` (default,
//!   `{"modalities": [kind, "text"]}`, OpenRouter's form) or `tool`
//!   (`{"tools": [{"type": "image_generation"}]}`, OpenAI's hosted tool).
//!   `generateParams` is merged last as an escape hatch.
//! - `command`: uses the command-provider transport with reference attachments and binary outputFile.
use super::{CommitBarrier, PermissionGate, PreparedOperation, ToolDefinition};
use crate::spill::{self, Document, Limits, Spill, Spilled, Text};
use crate::{paths::Resolver, schema::Tokens, schema::UsageRecord};
use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use code_config::{
    ConfigStore, ImageRequest, Modality, ModelConfig, Provider, ProviderType, ReportedCost,
};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::{
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(300);
const MAX_INPUT: u64 = 20 * 1024 * 1024;

pub(crate) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "generate".into(),
        description: "Generate an image, video or audio file with a media model and write it to outPath (default: the session assets directory). Returns the written path. Writing outside cwd needs the same approval as write.".into(),
        schema: json!({"type":"object","properties":{
            "kind":{"type":"string","enum":["image","video","audio"]},
            "prompt":{"type":"string"},
            "model":{"type":"string","description":"provider/model with this output kind; default: the first enabled one"},
            "inputs":{"type":"array","items":{"type":"string"},"description":"Reference image paths"},
            "outPath":{"type":"string","description":"Destination path (absolute or relative to cwd); must not exist yet"}
        },"required":["kind","prompt"],"additionalProperties":false}),
    }
}

fn media(kind: &Modality) -> bool {
    matches!(kind, Modality::Image | Modality::Video | Modality::Audio)
}

/// Whether `generate` belongs in a new agent's `tools[]` (fixed afterwards).
pub(crate) fn offered(snapshot: &ConfigStore) -> bool {
    snapshot
        .models()
        .any(|(_, p, _, m)| p.enabled && m.capabilities.output.iter().any(media))
}

pub(crate) struct Context<'a> {
    pub control: &'a crate::Control,
    pub gate: &'a dyn PermissionGate,
    pub barrier: &'a CommitBarrier,
    pub op: PreparedOperation,
    pub snapshot: ConfigStore,
    pub session_id: String,
    pub agent_id: String,
    pub assets: PathBuf,
    /// The session's private temp directory (created 0700 on first spill).
    pub temp: PathBuf,
    pub cancel: &'a CancellationToken,
}

/// Usage is recorded inside `execute` (before the output is published), so
/// a billed generation counts even when publishing is cancelled.
pub(crate) struct Generated {
    pub output: String,
}

pub(crate) async fn execute(ctx: Context<'_>) -> Result<Generated> {
    let limits = Limits::new(ctx.snapshot.config().generate.max_output_bytes);
    execute_with(ctx, limits).await
}

async fn execute_with(mut ctx: Context<'_>, limits: Limits) -> Result<Generated> {
    let args = ctx.op.call.arguments.clone();
    let kind = match args["kind"].as_str() {
        Some("image") => Modality::Image,
        Some("video") => Modality::Video,
        Some("audio") => Modality::Audio,
        _ => bail!("kind must be image, video or audio"),
    };
    let kind_name = args["kind"].as_str().unwrap();
    let prompt = args["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .context("prompt is required")?
        .to_owned();
    let (provider_id, model_id, provider, model) =
        choose(&ctx.snapshot, args["model"].as_str(), &kind, kind_name)?;
    let full_id = format!("{provider_id}/{model_id}");
    crate::exclusive::check_request(&ctx.session_id, "generate", &full_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let resolver = Resolver::new(ctx.op.cwd.clone());
    let out = args["outPath"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
        .map(|p| resolver.resolve(p))
        .transpose()?;
    if let Some(path) = &out {
        resolver.validate(path)?;
        if path.exists() {
            bail!(
                "{} already exists; choose a new outPath (or delete the old file with bash)",
                path.display()
            );
        }
    }
    let mut inputs = Vec::new();
    for raw in args["inputs"].as_array().into_iter().flatten() {
        let path = resolver.resolve(raw.as_str().context("inputs must be paths")?)?;
        if !path.starts_with(&ctx.op.cwd) {
            // Reading a reference outside cwd follows `read` rules.
            let mut read = ctx.op.clone();
            read.call.name = "read".into();
            read.requested.name = "read".into();
            read.call.arguments = json!({"path": path});
            read.requested.arguments = read.call.arguments.clone();
            read.target = Some(path.clone());
            read.command = None;
            ctx.gate.authorize(&read, ctx.cancel).await?;
        }
        inputs.push(path);
    }
    // A destination follows `write` rules: outside cwd manual asks; in auto and
    // unattended the sidecar reviews it inside cwd too. The default session
    // assets directory is cockpit-code's own storage and is not gated.
    ctx.op.target = out.clone();
    ctx.op.call.arguments["model"] = json!(full_id);
    ctx.gate.authorize(&ctx.op, ctx.cancel).await?;
    if ctx.cancel.is_cancelled() {
        bail!("cancelled");
    }

    let kind_wire = provider_kind(&provider, &model);
    // One deadline covers the request and the media decode after it.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let timed_out = || anyhow!("generation timed out after {}s", TIMEOUT.as_secs());
    let (response, usage) = match kind_wire {
        ProviderType::OpenaiResponses | ProviderType::OpenaiChat => {
            let request = http_request(kind_wire, &model, &model_id, kind_name, &prompt, &inputs)?;
            let audio_format = request["audio"]["format"]
                .as_str()
                .unwrap_or("wav")
                .to_owned();
            let (value, document, usage) = tokio::select! {
                _ = ctx.cancel.cancelled() => bail!("cancelled"),
                r = tokio::time::timeout_at(deadline, send(kind_wire, &provider, &model, request, &ctx.temp, limits, ctx.cancel)) =>
                    r.map_err(|_| timed_out())??,
            };
            ((value, document, audio_format), usage)
        }
        ProviderType::Command => {
            return command_provider(
                &ctx,
                &provider_id,
                &model_id,
                &provider,
                &model,
                &prompt,
                &inputs,
                out.as_deref(),
                kind_name,
                limits,
            )
            .await;
        }
        ProviderType::Anthropic => {
            bail!(
                "{full_id}: the anthropic wire has no media output; use openai-responses, openai-chat or a command provider"
            )
        }
    };
    if let Some((tokens, cost)) = usage {
        let usage = UsageRecord {
            call_id: uuid::Uuid::new_v4().to_string(),
            session_id: ctx.session_id.clone(),
            agent_id: ctx.agent_id.clone(),
            provider: provider_id.clone(),
            endpoint: format!("{provider_id}:{kind_wire:?}"),
            attempt: 0,
            input_estimate: None,
            model: full_id.clone(),
            kind: "generate".into(),
            tokens,
            cost_usd: cost,
            partial: false,
            context_used: None,
            context_window: None,
            quota: None,
        };
        // The provider has billed this request even if publishing its output
        // fails. Command providers record usage in Control::auxiliary above.
        ctx.control.commit(
            &ctx.agent_id,
            crate::schema::HistoryEvent::Usage {
                usage: usage.clone(),
            },
        )?;
        ctx.control
            .emit(crate::schema::EngineEvent::Usage { usage });
    }
    // Usage is committed before decoding: a billed response whose media
    // fails to decode, exceeds a limit or is cancelled still counts.
    let (value, document, audio_format) = response;
    let media = tokio::select! {
        _ = ctx.cancel.cancelled() => bail!("cancelled"),
        r = tokio::time::timeout_at(deadline, decode_media(kind_wire, value, document, audio_format, &ctx.temp, limits, ctx.cancel)) =>
            r.map_err(|_| timed_out())??,
    };
    let Some(Media { data, mime }) = media else {
        bail!("{full_id} returned no {kind_name} output");
    };
    let path = match out {
        Some(path) => path,
        None => {
            std::fs::create_dir_all(&ctx.assets)?;
            ctx.assets.join(format!(
                "{}.{}",
                uuid::Uuid::new_v4(),
                extension(&mime, kind_name)
            ))
        }
    };
    let bytes = write_new_async(
        &path,
        data.into_reader()?,
        limits.ceiling,
        "generated media",
        ctx.barrier,
        ctx.gate,
        ctx.cancel,
    )
    .await?;
    Ok(Generated {
        output: json!({"path": path, "model": full_id, "mimeType": mime, "bytes": bytes})
            .to_string(),
    })
}

fn provider_kind(provider: &Provider, model: &ModelConfig) -> ProviderType {
    model.kind.unwrap_or(provider.kind)
}

fn choose(
    snapshot: &ConfigStore,
    requested: Option<&str>,
    kind: &Modality,
    kind_name: &str,
) -> Result<(String, String, Provider, ModelConfig)> {
    let candidates = snapshot
        .models()
        .filter(|(_, p, _, m)| p.enabled && m.capabilities.output.contains(kind))
        .map(|(pid, p, mid, m)| (pid.clone(), mid.clone(), p.clone(), m.clone()))
        .collect::<Vec<_>>();
    let ids = candidates
        .iter()
        .map(|(p, m, _, _)| format!("{p}/{m}"))
        .collect::<Vec<_>>();
    let available = || {
        if ids.is_empty() {
            format!("no enabled model outputs {kind_name}")
        } else {
            format!("models with {kind_name} output: {}", ids.join(", "))
        }
    };
    match requested.filter(|s| !s.is_empty()) {
        None => candidates.into_iter().next().with_context(available),
        Some(id) => {
            let (pid, mid) = id.split_once('/').context("model must be provider/model")?;
            candidates
                .iter()
                .find(|(p, m, _, _)| p == pid && m == mid)
                .cloned()
                .with_context(|| {
                    format!("{id} is not an enabled {kind_name} model; {}", available())
                })
        }
    }
}

/// Opens a reference input without blocking on a FIFO or device.
fn open_regular(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("{}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    Ok(file)
}

fn data_url(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    if meta.len() > MAX_INPUT {
        bail!("{} is larger than 20 MiB", path.display());
    }
    // Bounded read: the file may have grown since the metadata check.
    let mut bytes = Vec::new();
    open_regular(path)?
        .take(MAX_INPUT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INPUT {
        bail!("{} is larger than 20 MiB", path.display());
    }
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => bail!(
            "{}: inputs must be png, jpeg, webp or gif images",
            path.display()
        ),
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

fn http_request(
    wire: ProviderType,
    model: &ModelConfig,
    model_id: &str,
    kind_name: &str,
    prompt: &str,
    inputs: &[PathBuf],
) -> Result<Value> {
    let images = inputs
        .iter()
        .map(|p| data_url(p))
        .collect::<Result<Vec<_>>>()?;
    let mut body = if wire == ProviderType::OpenaiResponses {
        let mut content = vec![json!({"type":"input_text","text":prompt})];
        content.extend(
            images
                .iter()
                .map(|url| json!({"type":"input_image","image_url":url})),
        );
        json!({"model": model_id, "input": [{"role":"user","content":content}]})
    } else {
        let mut content = vec![json!({"type":"text","text":prompt})];
        content.extend(
            images
                .iter()
                .map(|url| json!({"type":"image_url","image_url":{"url":url}})),
        );
        json!({"model": model_id, "messages": [{"role":"user","content":content}]})
    };
    let mut params = match model.image_request {
        ImageRequest::Tool if kind_name == "image" => {
            json!({"tools": [{"type": "image_generation"}]})
        }
        ImageRequest::Tool => bail!("imageRequest \"tool\" only generates images"),
        ImageRequest::Modalities => json!({"modalities": [kind_name, "text"]}),
    };
    match model.generate_params.clone() {
        Some(Value::Object(extra)) => params.as_object_mut().unwrap().extend(extra),
        Some(_) => bail!("generateParams must be a JSON object"),
        None => {}
    }
    let object: &mut Map<String, Value> = body.as_object_mut().unwrap();
    for (k, v) in params.as_object().cloned().unwrap() {
        if matches!(k.as_str(), "model" | "input" | "messages") {
            bail!("generateParams may not override {k}");
        }
        object.insert(k, v);
    }
    Ok(body)
}

type Usage = Option<(Tokens, Option<f64>)>;

/// Decoded media: in memory up to `Limits::media_cap`, else an unnamed temp file.
struct Media {
    data: Spilled,
    mime: String,
}

#[allow(clippy::too_many_arguments)]
async fn send(
    wire: ProviderType,
    provider: &Provider,
    model: &ModelConfig,
    body: Value,
    temp: &Path,
    limits: Limits,
    cancel: &CancellationToken,
) -> Result<(Value, Document, Usage)> {
    let base = provider
        .connection_url()
        .context("provider has no baseUrl")?
        .trim_end_matches('/');
    let suffix = if wire == ProviderType::OpenaiResponses {
        "/responses"
    } else {
        "/chat/completions"
    };
    let client = crate::http_client(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
    })?;
    let mut request = client.post(format!("{base}{suffix}")).json(&body);
    for (k, v) in provider.connection_headers() {
        request = request.header(k.as_str(), v.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("provider network error"))?;
    let status = response.status();
    let header_cost = match provider.reported_cost(model) {
        Some(ReportedCost::Header { name, unit }) => response
            .headers()
            .get(name)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<f64>().ok())
            .and_then(|v| unit.dollars(v)),
        _ => None,
    };
    // Blocking work below stops at its next check once this future is
    // dropped (cancellation or timeout); its spill files close with it.
    let token = cancel.child_token();
    let _stop = token.clone().drop_guard();
    let network = |_| anyhow!("provider network error");
    let (value, document) = if status.is_success() {
        // Disk writes run on a blocking thread fed through a bounded channel.
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut writer = spill_writer(rx, temp.to_path_buf(), limits);
        let mut stream = response.bytes_stream();
        let mut failed = None;
        loop {
            tokio::select! {
                // The writer only ends early on an error (such as the
                // ceiling); report it at once, even if the provider stalls.
                done = &mut writer => {
                    return Err(match done {
                        Ok(Err(error)) => error,
                        _ => anyhow!("generation response task failed"),
                    });
                }
                chunk = stream.next() => match chunk {
                    None => break,
                    Some(Ok(chunk)) => {
                        if tx.send(chunk).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        failed = Some(network(e));
                        break;
                    }
                },
            }
        }
        drop(tx);
        let body = writer
            .await
            .map_err(|_| anyhow!("generation response task failed"))??;
        if let Some(error) = failed {
            return Err(error);
        }
        let token = token.clone();
        tokio::task::spawn_blocking(move || Document::parse(body, limits, &token))
            .await
            .map_err(|_| anyhow!("generation response task failed"))??
    } else {
        let text = crate::model::error_body_prefix(response.bytes_stream())
            .await
            .map_err(network)?;
        Document::parse(Spilled::Memory(text.into_bytes()), limits, &token)?
    };
    if !status.is_success() || value.get("error").is_some_and(|e| !e.is_null()) {
        let message = value["error"]["message"]
            .as_str()
            .or_else(|| value["error"].as_str())
            .map(|s| document.prefix(s, 400))
            .unwrap_or_else(|| "request failed".into());
        bail!("generation failed (HTTP {}): {message}", status.as_u16());
    }
    let usage = &value["usage"];
    let tokens = Tokens {
        input_tokens: usage["input_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens"].as_u64()),
        output_tokens: usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64()),
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
    };
    let body_cost = match provider.reported_cost(model) {
        Some(ReportedCost::Body { pointer, unit }) => value
            .pointer(pointer)
            .and_then(Value::as_f64)
            .and_then(|v| unit.dollars(v)),
        _ => None,
    };
    let rated = model.cost.as_ref().and_then(|rates| {
        Some(
            tokens.input_tokens? as f64 * rates.input_per_m_tok? / 1e6
                + tokens.output_tokens? as f64 * rates.output_per_m_tok? / 1e6,
        )
    });
    Ok((
        value,
        document,
        Some((tokens, header_cost.or(body_cost).or(rated))),
    ))
}

/// Decodes the response's media on a blocking thread; dropping this future
/// stops it at its next check, which also releases the response spill.
async fn decode_media(
    wire: ProviderType,
    value: Value,
    document: Document,
    audio_format: String,
    temp: &Path,
    limits: Limits,
    cancel: &CancellationToken,
) -> Result<Option<Media>> {
    let token = cancel.child_token();
    let _stop = token.clone().drop_guard();
    let temp = temp.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let decoder = Decoder {
            document: &document,
            temp: &temp,
            limits,
            cancel: &token,
        };
        if wire == ProviderType::OpenaiResponses {
            responses_media(&value, &decoder)
        } else {
            chat_media(&value, &decoder, &audio_format)
        }
    })
    .await
    .map_err(|_| anyhow!("generation response task failed"))?
}

/// Receives response chunks on a blocking thread so spilling to disk never
/// stalls an async worker. Dropping the sender ends it; an error (such as
/// the ceiling) drops the receiver, which stops the download.
fn spill_writer<B: AsRef<[u8]> + Send + 'static>(
    mut rx: tokio::sync::mpsc::Receiver<B>,
    temp: PathBuf,
    limits: Limits,
) -> tokio::task::JoinHandle<Result<Spilled>> {
    tokio::task::spawn_blocking(move || {
        let mut spill = Spill::new(
            &temp,
            limits.text_cap,
            limits.ceiling,
            "generation response",
        );
        while let Some(chunk) = rx.blocking_recv() {
            spill.write(chunk.as_ref())?;
        }
        spill.finish()
    })
}

struct Decoder<'a> {
    document: &'a Document,
    temp: &'a Path,
    limits: Limits,
    cancel: &'a CancellationToken,
}
impl Decoder<'_> {
    /// Media from a base64 string or a `data:<mime>;base64,` URL, decoded
    /// as a stream into memory or, past the media cap, a spill file.
    fn decode(&self, raw: &str, default_mime: &str) -> Result<Media> {
        let mut out = Spill::new(
            self.temp,
            self.limits.media_cap,
            self.limits.ceiling,
            "generated media",
        );
        let declared = match self.document.text(raw) {
            Text::Inline(raw) => {
                let (declared, data) = match raw.strip_prefix("data:") {
                    Some(rest) => {
                        let (meta, data) = rest.split_once(',').context("malformed data URL")?;
                        (meta.split(';').next().filter(|m| !m.is_empty()), data)
                    }
                    None => (None, raw),
                };
                spill::decode_base64(data.trim().as_bytes(), &mut out, self.cancel)?;
                declared.map(str::to_owned)
            }
            Text::Stream(reader) => {
                let (declared, rest) = spill::data_url(reader, self.cancel)?;
                spill::decode_base64(spill::Trim::new(rest, self.cancel), &mut out, self.cancel)?;
                declared
            }
        };
        let data = out.finish()?;
        if data.len() == 0 {
            bail!("media output is empty");
        }
        // `sniff` reads at most 12 bytes; `head.len() > n` iff `data.len() > n`.
        let head = data.head(16)?;
        let mime = declared
            .or_else(|| sniff(&head).map(str::to_owned))
            .unwrap_or_else(|| default_mime.to_owned());
        Ok(Media { data, mime })
    }
}

fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if bytes.starts_with(b"ID3") || bytes.starts_with(&[0xff, 0xfb]) {
        Some("audio/mpeg")
    } else if bytes.len() > 8 && &bytes[4..8] == b"ftyp" {
        Some("video/mp4")
    } else {
        None
    }
}

fn responses_media(value: &Value, decoder: &Decoder) -> Result<Option<Media>> {
    for item in value["output"].as_array().into_iter().flatten() {
        if item["type"] == "image_generation_call"
            && let Some(result) = item["result"].as_str()
        {
            return decoder.decode(result, "image/png").map(Some);
        }
        for part in item["content"].as_array().into_iter().flatten() {
            if let Some(url) = part["image_url"]
                .as_str()
                .filter(|u| decoder.document.prefix(u, 5) == "data:")
            {
                return decoder.decode(url, "image/png").map(Some);
            }
        }
    }
    Ok(None)
}

fn chat_media(value: &Value, decoder: &Decoder, format: &str) -> Result<Option<Media>> {
    let message = &value["choices"][0]["message"];
    for image in message["images"].as_array().into_iter().flatten() {
        if let Some(url) = image["image_url"]["url"].as_str() {
            return decoder.decode(url, "image/png").map(Some);
        }
    }
    if let Some(data) = message["audio"]["data"].as_str() {
        let mime = match format {
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "opus" => "audio/opus",
            "aac" => "audio/aac",
            _ => "audio/wav",
        };
        return decoder.decode(data, mime).map(Some);
    }
    Ok(None)
}

fn extension(mime: &str, kind: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" => "opus",
        "audio/aac" => "aac",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        _ => match kind {
            "image" => "png",
            "audio" => "wav",
            _ => "mp4",
        },
    }
}

/// Copies `source` (at most `ceiling` bytes) into a new temp file in `parent`.
fn stage(
    parent: &Path,
    source: &mut dyn Read,
    ceiling: u64,
    what: &str,
    cancel: &CancellationToken,
) -> Result<(tempfile::NamedTempFile, u64)> {
    std::fs::create_dir_all(parent)?;
    let mut temp = super::new_file_temp(parent)?;
    let mut buf = vec![0; 64 * 1024];
    let mut total = 0u64;
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled before commit");
        }
        let n = match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        total += n as u64;
        if total > ceiling {
            return Err(spill::ceiling_error(what, ceiling));
        }
        temp.write_all(&buf[..n])?;
    }
    temp.as_file().sync_all()?;
    Ok((temp, total))
}

/// Atomic create: never replaces an existing file. Returns the bytes written.
#[cfg(test)]
fn write_new(
    path: &Path,
    source: &mut dyn Read,
    ceiling: u64,
    what: &str,
    barrier: &CommitBarrier,
    gate: &dyn PermissionGate,
    cancel: &CancellationToken,
) -> Result<u64> {
    let parent = path.parent().context("path has no parent")?;
    let (temp, total) = stage(parent, source, ceiling, what, cancel)?;
    publish(temp, path, barrier, gate, cancel)?;
    Ok(total)
}

/// [`write_new`] with the copy on a blocking thread; dropping this future
/// stops the copy at its next chunk and removes the staged temp file.
#[allow(clippy::too_many_arguments)]
async fn write_new_async(
    path: &Path,
    mut source: Box<dyn Read + Send>,
    ceiling: u64,
    what: &'static str,
    barrier: &CommitBarrier,
    gate: &dyn PermissionGate,
    cancel: &CancellationToken,
) -> Result<u64> {
    let parent = path.parent().context("path has no parent")?.to_path_buf();
    let token = cancel.child_token();
    let _stop = token.clone().drop_guard();
    let (temp, total) =
        tokio::task::spawn_blocking(move || stage(&parent, &mut *source, ceiling, what, &token))
            .await
            .map_err(|_| anyhow!("generation output task failed"))??;
    publish(temp, path, barrier, gate, cancel)?;
    Ok(total)
}

fn publish(
    temp: tempfile::NamedTempFile,
    path: &Path,
    barrier: &CommitBarrier,
    gate: &dyn PermissionGate,
    cancel: &CancellationToken,
) -> Result<()> {
    barrier.commit(|| {
        gate.commit(Box::new(|| {
            if cancel.is_cancelled() {
                bail!("cancelled before commit");
            }
            temp.persist_noclobber(path)
                .map_err(|e| anyhow::anyhow!("{}: {}", path.display(), e.error))?;
            Ok(())
        }))
    })
}

/// Media files use the same argv, timeout, process group and usage transport as inference.
#[allow(clippy::too_many_arguments)]
async fn command_provider(
    ctx: &Context<'_>,
    provider_id: &str,
    model_id: &str,
    provider: &Provider,
    config: &ModelConfig,
    prompt: &str,
    inputs: &[PathBuf],
    out: Option<&Path>,
    kind: &str,
    limits: Limits,
) -> Result<Generated> {
    let dir = tempfile::tempdir()?;
    let assets = dir.path().join("assets");
    std::fs::create_dir(&assets)?;
    // Copied on a blocking thread, streamed and bounded; the private tempdir
    // is removed on every exit.
    let attachments = {
        let inputs = inputs.to_vec();
        let assets = assets.clone();
        let token = ctx.cancel.child_token();
        let _stop = token.clone().drop_guard();
        tokio::task::spawn_blocking(move || {
            let mut attachments = Vec::new();
            for path in &inputs {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("invalid reference filename")?;
                let mut source = open_regular(path)?.take(MAX_INPUT + 1);
                let stored = format!("{}-{}", uuid::Uuid::new_v4(), name);
                let mut copy = std::fs::File::create(assets.join(&stored))?;
                let mut buf = vec![0; 64 * 1024];
                let mut total = 0;
                loop {
                    if token.is_cancelled() {
                        bail!("cancelled");
                    }
                    let n = source.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    total += n as u64;
                    copy.write_all(&buf[..n])?;
                }
                if total > MAX_INPUT {
                    bail!("reference input exceeds 20 MiB");
                }
                attachments.push(crate::schema::Attachment {
                    path: stored,
                    name: name.into(),
                    mime_type: crate::attachments::mime_type(name).into(),
                });
            }
            anyhow::Ok(attachments)
        })
        .await
        .map_err(|_| anyhow!("reference copy task failed"))??
    };
    let mut transport_config = config.clone();
    transport_config.capabilities.output = vec![Modality::Text];
    transport_config.capabilities.tools = false;
    let model = crate::model::Model::new(
        provider_id.into(),
        model_id.into(),
        provider.clone(),
        transport_config,
        None,
    )?;
    let mut request = model.prepare_with_assets(
        "",
        &[],
        &[crate::schema::HistoryEvent::User {
            text: prompt.into(),
            attachments,
        }],
        &ctx.session_id,
        Some(assets),
    )?;
    request.agent_id = ctx.agent_id.clone();
    request.kind = "generate".into();
    request.retry_limit = 0;
    request.command_context.cwd = ctx.op.cwd.clone();
    let output = dir.path().join(format!("output.{}", extension("", kind)));
    request.command_context.output_file = Some(output.clone());
    ctx.control.auxiliary(&model, &request, ctx.cancel).await?;
    let metadata =
        std::fs::symlink_metadata(&output).context("command provider did not write outputFile")?;
    if !metadata.is_file() || metadata.len() == 0 {
        bail!("command outputFile must be a nonempty regular file");
    }
    if metadata.len() > limits.ceiling {
        return Err(spill::ceiling_error("command outputFile", limits.ceiling));
    }
    // Streamed straight from the provider's private tempdir to the
    // destination; only the first bytes are read for type sniffing.
    let mut file = {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&output)
            .context("command outputFile must be a nonempty regular file")?;
        let opened = file.metadata()?;
        if !opened.is_file() || (opened.dev(), opened.ino()) != (metadata.dev(), metadata.ino()) {
            bail!("command outputFile changed while it was being read");
        }
        file
    };
    let mut bytes = Vec::new();
    (&mut file).take(16).read_to_end(&mut bytes)?;
    file.rewind()?;
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        "audio/wav"
    } else if bytes.starts_with(b"ID3")
        || (bytes.first() == Some(&0xff) && bytes.get(1).is_some_and(|b| b & 0xe0 == 0xe0))
    {
        "audio/mpeg"
    } else if bytes.get(4..8) == Some(b"ftyp") {
        "video/mp4"
    } else if bytes.starts_with(b"\x1a\x45\xdf\xa3") {
        "video/webm"
    } else {
        "application/octet-stream"
    };
    let path = match out {
        Some(path) => path.to_path_buf(),
        None => {
            std::fs::create_dir_all(&ctx.assets)?;
            ctx.assets.join(format!(
                "{}.{}",
                uuid::Uuid::new_v4(),
                if mime == "application/octet-stream" {
                    "bin"
                } else {
                    extension(mime, kind)
                }
            ))
        }
    };
    let bytes = write_new_async(
        &path,
        Box::new(file),
        limits.ceiling,
        "command outputFile",
        ctx.barrier,
        ctx.gate,
        ctx.cancel,
    )
    .await?;
    Ok(Generated {
        output: json!({"path":path,"model":model.id(),"mimeType":mime,"bytes":bytes}).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation_session(provider: Value) -> (tempfile::TempDir, crate::Session) {
        generation_session_with(provider, json!({}))
    }

    fn generation_session_with(
        provider: Value,
        config: Value,
    ) -> (tempfile::TempDir, crate::Session) {
        let (dir, store) = crate::test_support::store(config);
        let providers = store.directories().config.join("providers");
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(providers.join("p.json"), provider.to_string()).unwrap();
        let store = ConfigStore::load(store.directories()).unwrap();
        assert!(store.errors().is_empty(), "{:?}", store.errors());
        let session = crate::Session::create(store, dir.path().into(), Default::default()).unwrap();
        (dir, session)
    }

    async fn generate_for_test(
        session: &crate::Session,
        gate: &dyn PermissionGate,
        barrier: &CommitBarrier,
        cancel: &CancellationToken,
        path: &Path,
    ) -> Result<Generated> {
        let snapshot = session.control.config.lock().unwrap().clone();
        execute(Context {
            control: &session.control,
            gate,
            barrier,
            op: PreparedOperation::metadata(
                crate::schema::ToolCall {
                    id: "call_0".into(), name: "generate".into(), complete: true,
                    arguments: json!({"kind":"image", "prompt":"test image", "model":"p/m", "outPath":path}),
                },
                session.cwd(), None,
            ),
            snapshot,
            session_id: session.id(), agent_id: "root".into(),
            assets: session.cwd().join("assets"), temp: session.cwd().join("tmp"), cancel,
        }).await
    }

    async fn http_provider(requests: usize) -> (Value, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..requests {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    loop {
                        let mut bytes = [0; 4096];
                        let n = socket.read(&mut bytes).await.unwrap();
                        assert_ne!(n, 0);
                        request.extend_from_slice(&bytes[..n]);
                        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length: usize = headers.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                            }).unwrap();
                            if request.len() >= end + 4 + length { break; }
                        }
                    }
                    let body = json!({
                        "output":[{"type":"image_generation_call", "result":"aW1hZ2U="}],
                        "usage":{"input_tokens":10, "output_tokens":5}, "cost":0.25,
                    }).to_string();
                    socket.write_all(format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                    ).as_bytes()).await.unwrap();
                }).await.expect("generation request timed out");
            }
        });
        (
            json!({
                "type":"openai-responses", "baseUrl":format!("http://{address}"),
                "reportedCost":{"from":"body", "pointer":"/cost", "unit":"usd"},
                "models":{"m":{"capabilities":{"output":["image"]}}},
            }),
            server,
        )
    }

    #[cfg(unix)]
    fn command_provider_config() -> Value {
        json!({
            "type":"command", "command":{
                "argv":["sh", "-c", r#"printf image > "$1"; printf '%s\n' '{"type":"usage","inputTokens":10,"outputTokens":5,"costUsd":0.25}' '{"type":"done","stopReason":"end_turn"}'"#, "adapter", "{outputFile}"],
                "output":"jsonl",
            },
            "models":{"m":{"capabilities":{"output":["image"]}}},
        })
    }

    async fn assert_usage_survives_failed_commits(provider: Value) {
        let (dir, session) = generation_session(provider);
        for mode in 0..3 {
            let cancel = CancellationToken::new();
            let barrier = CommitBarrier::default();
            if mode == 2 {
                barrier.revoke();
            }
            let gate = CommitGate {
                reject: mode == 0,
                cancel: (mode == 1).then(|| cancel.clone()),
            };
            let path = dir.path().join(format!("failed-{mode}.png"));
            let error = generate_for_test(&session, &gate, &barrier, &cancel, &path)
                .await
                .err()
                .unwrap();
            assert!(
                error.to_string().contains(if mode == 0 {
                    "session ownership lost"
                } else {
                    "cancelled before commit"
                }),
                "{error:#}"
            );
            assert!(!path.exists());
            let records = session.usage_records();
            assert_eq!(records.len(), mode + 1);
            assert_eq!(session.totals().known_cost_usd, (mode + 1) as f64 * 0.25);
            assert_eq!(records[mode].tokens.input_tokens, Some(10));
            assert_eq!(records[mode].cost_usd, Some(0.25));
        }
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn http_usage_is_recorded_before_output_commit_can_fail() {
        let (provider, server) = http_provider(3).await;
        assert_usage_survives_failed_commits(provider).await;
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_usage_is_recorded_before_output_commit_can_fail() {
        assert_usage_survives_failed_commits(command_provider_config()).await;
    }

    async fn assert_reused_tool_ids_have_distinct_usage(provider: Value) {
        let (dir, session) = generation_session(provider);
        for index in 0..2 {
            let path = dir.path().join(format!("image-{index}.png"));
            generate_for_test(
                &session,
                &super::super::YoloGate,
                &CommitBarrier::default(),
                &CancellationToken::new(),
                &path,
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read(path).unwrap(), b"image");
        }
        let records = session.usage_records();
        assert_eq!(records.len(), 2);
        assert_ne!(records[0].call_id, records[1].call_id);
        for record in records {
            assert!(uuid::Uuid::parse_str(&record.call_id).is_ok());
            assert_eq!(record.kind, "generate");
            assert_eq!(record.cost_usd, Some(0.25));
        }
        assert_eq!(session.totals().calls, 2);
        assert_eq!(session.totals().known_cost_usd, 0.5);
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn http_generations_with_reused_tool_ids_have_distinct_usage() {
        let (provider, server) = http_provider(2).await;
        assert_reused_tool_ids_have_distinct_usage(provider).await;
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_generations_with_reused_tool_ids_have_distinct_usage() {
        assert_reused_tool_ids_have_distinct_usage(command_provider_config()).await;
    }

    struct CommitGate {
        reject: bool,
        cancel: Option<CancellationToken>,
    }
    #[async_trait::async_trait]
    impl PermissionGate for CommitGate {
        async fn authorize(
            &self,
            _operation: &PreparedOperation,
            _cancel: &CancellationToken,
        ) -> Result<()> {
            Ok(())
        }
        fn commit(&self, action: Box<dyn FnOnce() -> Result<()> + Send + '_>) -> Result<()> {
            if self.reject {
                bail!("session ownership lost");
            }
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            action()
        }
    }

    #[test]
    fn generated_file_is_not_published_after_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.png");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = write_new(
            &path,
            &mut &b"image"[..],
            u64::MAX,
            "test",
            &CommitBarrier::default(),
            &super::super::YoloGate,
            &cancel,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled before commit"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn generated_file_is_not_published_after_parent_barrier_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.png");
        let parent = std::sync::Arc::new(CommitBarrier::default());
        let barrier = CommitBarrier::linked(parent.clone());
        parent.revoke();
        let error = write_new(
            &path,
            &mut &b"image"[..],
            u64::MAX,
            "test",
            &barrier,
            &super::super::YoloGate,
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled before commit"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn generated_file_is_not_published_after_session_ownership_is_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.png");
        let gate = CommitGate {
            reject: true,
            cancel: None,
        };
        let error = write_new(
            &path,
            &mut &b"image"[..],
            u64::MAX,
            "test",
            &CommitBarrier::default(),
            &gate,
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("session ownership lost"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn generated_file_checks_cancellation_inside_session_commit_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.png");
        let cancel = CancellationToken::new();
        let gate = CommitGate {
            reject: false,
            cancel: Some(cancel.clone()),
        };
        let error = write_new(
            &path,
            &mut &b"image"[..],
            u64::MAX,
            "test",
            &CommitBarrier::default(),
            &gate,
            &cancel,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled before commit"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn generated_file_is_published_without_replacing_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.png");
        let barrier = CommitBarrier::default();
        let cancel = CancellationToken::new();
        let yolo = &super::super::YoloGate;
        assert_eq!(
            write_new(
                &path,
                &mut &b"first"[..],
                5,
                "test",
                &barrier,
                yolo,
                &cancel
            )
            .unwrap(),
            5
        );
        assert!(
            write_new(
                &path,
                &mut &b"second"[..],
                6,
                "test",
                &barrier,
                yolo,
                &cancel
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn decode(raw: &str, default_mime: &str) -> Result<(Vec<u8>, String)> {
        let temp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let (_, document) = Document::parse(
            Spilled::Memory(b"null".to_vec()),
            Limits::new(u64::MAX),
            &cancel,
        )?;
        let decoder = Decoder {
            document: &document,
            temp: temp.path(),
            limits: Limits::new(u64::MAX),
            cancel: &cancel,
        };
        let media = decoder.decode(raw, default_mime)?;
        let mut bytes = Vec::new();
        media.data.into_reader()?.read_to_end(&mut bytes)?;
        Ok((bytes, media.mime))
    }

    #[test]
    fn base64_media_decodes_padded_standard_and_data_urls() {
        assert_eq!(
            decode(" AP8= ", "application/octet-stream").unwrap(),
            (vec![0, 255], "application/octet-stream".into())
        );
        assert_eq!(
            decode("data:image/png;base64,AP8=", "fallback").unwrap(),
            (vec![0, 255], "image/png".into())
        );
        for value in ["", "AP8", "AP_=", "data:image/png", "!!!!"] {
            assert!(decode(value, "fallback").is_err(), "{value}");
        }
    }

    async fn generate_with(
        session: &crate::Session,
        cancel: &CancellationToken,
        path: &Path,
        limits: Limits,
    ) -> Result<Value> {
        let snapshot = session.control.config.lock().unwrap().clone();
        let generated = execute_with(Context {
            control: &session.control,
            gate: &super::super::YoloGate,
            barrier: &CommitBarrier::default(),
            op: PreparedOperation::metadata(
                crate::schema::ToolCall {
                    id: "call_0".into(), name: "generate".into(), complete: true,
                    arguments: json!({"kind":"image", "prompt":"test image", "model":"p/m", "outPath":path}),
                },
                session.cwd(), None,
            ),
            snapshot,
            session_id: session.id(), agent_id: "root".into(),
            assets: session.cwd().join("assets"), temp: session.cwd().join("tmp"), cancel,
        }, limits).await?;
        Ok(serde_json::from_str(&generated.output).unwrap())
    }

    /// One HTTP exchange serving `body`; with `stall`, only the first
    /// `stall` bytes are sent and the connection is held open.
    async fn serve(
        body: Vec<u8>,
        stall: Option<usize>,
        sent: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> (Value, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 4096];
                let n = socket.read(&mut bytes).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&bytes[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            let end = stall.unwrap_or(body.len());
            let _ = socket.write_all(&body[..end]).await;
            let _ = socket.flush().await;
            if let Some(sent) = sent {
                let _ = sent.send(());
            }
            if stall.is_some() {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        (
            json!({
                "type":"openai-responses", "baseUrl":format!("http://{address}"),
                "models":{"m":{"capabilities":{"output":["image"]}}},
            }),
            server,
        )
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend((8..len).map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        }));
        bytes
    }

    /// Open descriptors (spill files included: they have no name) under `dir`.
    /// Linux and macOS only, like the tests that use it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn open_files_under(dir: &Path) -> usize {
        // Both platforms report descriptor paths with symlinks resolved. `dir` may
        // not exist yet (it is created on first spill), but its parent does.
        let dir = std::fs::canonicalize(dir.parent().unwrap())
            .unwrap()
            .join(dir.file_name().unwrap());
        open_file_paths()
            .iter()
            .filter(|target| target.starts_with(&dir))
            .count()
    }

    #[cfg(target_os = "linux")]
    fn open_file_paths() -> Vec<PathBuf> {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok())
            .collect()
    }

    /// macOS has no /proc; `F_GETPATH` still reports an unlinked file's last path.
    #[cfg(target_os = "macos")]
    fn open_file_paths() -> Vec<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let fds: Vec<libc::c_int> = std::fs::read_dir("/dev/fd")
            .unwrap()
            .filter_map(|fd| fd.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        fds.into_iter()
            .filter_map(|fd| {
                let mut buf = [0u8; libc::MAXPATHLEN as usize];
                // SAFETY: F_GETPATH writes a NUL-terminated path of at most MAXPATHLEN bytes.
                if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
                    return None;
                }
                let len = buf.iter().position(|&b| b == 0)?;
                Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])))
            })
            .collect()
    }

    fn limits(text_cap: u64, media_cap: u64, ceiling: u64) -> Limits {
        Limits {
            text_cap,
            media_cap,
            ceiling,
            inline_string: 256,
        }
    }

    #[tokio::test]
    async fn http_media_under_the_caps_stays_in_memory() {
        let image = noise(30_000);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&image);
        let body = json!({"output":[{"type":"image_generation_call","result":encoded}]})
            .to_string()
            .into_bytes();
        let (provider, server) = serve(body.clone(), None, None).await;
        let (dir, session) = generation_session(provider);
        let path = dir.path().join("under.png");
        // Exactly at both caps: nothing spills.
        let output = generate_with(
            &session,
            &CancellationToken::new(),
            &path,
            limits(body.len() as u64, image.len() as u64, body.len() as u64),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), image);
        assert_eq!(output["bytes"], image.len());
        assert_eq!(output["mimeType"], "image/png");
        assert!(!session.cwd().join("tmp").exists());
        session.shutdown().await.unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn http_media_just_over_the_caps_spills_and_streams_base64() {
        let image = noise(30_000);
        let standard = base64::engine::general_purpose::STANDARD;
        for (index, variant) in ["result", "data-url"].into_iter().enumerate() {
            let encoded = standard.encode(&image);
            let body = if variant == "result" {
                // Escaped slashes and padding whitespace must survive the streamed path.
                format!(
                    r#"{{"output":[{{"type":"image_generation_call","result":" {} \n"}}],"usage":{{"input_tokens":3}}}}"#,
                    encoded.replace('/', "\\/")
                )
            } else {
                json!({"output":[{"content":[{"image_url":format!("data:image/webp;base64,{encoded}")}]}]})
                    .to_string()
            }
            .into_bytes();
            let (provider, server) = serve(body.clone(), None, None).await;
            let (dir, session) = generation_session(provider);
            let path = dir.path().join(format!("over-{index}.png"));
            let output = generate_with(
                &session,
                &CancellationToken::new(),
                &path,
                limits(
                    body.len() as u64 - 1,
                    image.len() as u64 - 1,
                    body.len() as u64,
                ),
            )
            .await
            .unwrap();
            server.await.unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), image, "{variant}");
            assert_eq!(output["bytes"], image.len());
            let mime = if variant == "result" {
                "image/png"
            } else {
                "image/webp"
            };
            assert_eq!(output["mimeType"], mime);
            let temp = session.cwd().join("tmp");
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&temp).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
            assert_eq!(open_files_under(&temp), 0);
            if variant == "result" {
                assert_eq!(session.usage_records()[0].tokens.input_tokens, Some(3));
            }
            session.shutdown().await.unwrap();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn http_response_over_the_ceiling_fails_clearly() {
        let image = noise(30_000);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&image);
        let body = json!({"output":[{"type":"image_generation_call","result":encoded}]})
            .to_string()
            .into_bytes();
        let (provider, server) = serve(body.clone(), None, None).await;
        let (dir, session) = generation_session(provider);
        let path = dir.path().join("ceiling.png");
        let error = generate_with(
            &session,
            &CancellationToken::new(),
            &path,
            limits(1024, 1024, body.len() as u64 - 1),
        )
        .await
        .unwrap_err()
        .to_string();
        server.abort();
        assert_eq!(
            error,
            format!(
                "generation response exceeds generate.maxOutputBytes ({} bytes); raise it in config.json if this output is expected",
                body.len() - 1
            )
        );
        assert!(!path.exists());
        let temp = session.cwd().join("tmp");
        assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
        assert_eq!(open_files_under(&temp), 0);
        session.shutdown().await.unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn cancelled_generation_releases_its_spill_file() {
        let image = noise(30_000);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&image);
        let body = json!({"output":[{"type":"image_generation_call","result":encoded}]})
            .to_string()
            .into_bytes();
        let (sent, sent_rx) = tokio::sync::oneshot::channel();
        let (provider, server) = serve(body.clone(), Some(body.len() / 2), Some(sent)).await;
        let (dir, session) = generation_session(provider);
        let path = dir.path().join("cancelled.png");
        let temp = session.cwd().join("tmp");
        let cancel = CancellationToken::new();
        let control = async {
            sent_rx.await.unwrap();
            // Wait until the partial body has spilled to an (unnamed) file.
            for _ in 0..200 {
                if open_files_under(&temp) == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(open_files_under(&temp), 1);
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(
            generate_with(&session, &cancel, &path, limits(1024, 1024, u64::MAX)),
            control
        );
        server.abort();
        assert_eq!(result.unwrap_err().to_string(), "cancelled");
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
        // The blocking writer closes the file once it sees the dropped sender.
        for _ in 0..200 {
            if open_files_under(&temp) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(open_files_under(&temp), 0);
        session.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_output_file_is_streamed_up_to_the_ceiling() {
        let (dir, session) = generation_session(command_provider_config());
        // `printf image` writes 5 bytes.
        let path = dir.path().join("exact.png");
        let output = generate_with(&session, &CancellationToken::new(), &path, limits(0, 0, 5))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"image");
        assert_eq!(output["bytes"], 5);
        let path = dir.path().join("over.png");
        let error = generate_with(&session, &CancellationToken::new(), &path, limits(0, 0, 4))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("command outputFile exceeds generate.maxOutputBytes (4 bytes)"),
            "{error}"
        );
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".tmp")
                })
                .count(),
            0
        );
        session.shutdown().await.unwrap();
    }

    #[test]
    fn staging_refuses_more_than_the_ceiling_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generated.png");
        let error = write_new(
            &path,
            &mut &b"123456"[..],
            5,
            "generated media",
            &CommitBarrier::default(),
            &super::super::YoloGate,
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("generated media exceeds generate.maxOutputBytes (5 bytes)"),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_max_output_bytes_is_the_ceiling() {
        let (dir, session) = generation_session_with(
            command_provider_config(),
            json!({"generate":{"maxOutputBytes":4}}),
        );
        let path = dir.path().join("configured.png");
        let error = generate_for_test(
            &session,
            &super::super::YoloGate,
            &CommitBarrier::default(),
            &CancellationToken::new(),
            &path,
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(
            error.starts_with("command outputFile exceeds generate.maxOutputBytes (4 bytes)"),
            "{error}"
        );
        assert!(!path.exists());
        session.shutdown().await.unwrap();
        let (_dir, store) = crate::test_support::store(json!({}));
        assert_eq!(
            store.config().generate.max_output_bytes,
            4 * 1024 * 1024 * 1024
        );
        std::fs::write(
            store.directories().config.join("config.json"),
            r#"{"generate":{"maxOutputBytes":0}}"#,
        )
        .unwrap();
        let error = ConfigStore::load(store.directories())
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("generate.maxOutputBytes must be positive"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn usage_is_recorded_when_billed_media_fails_to_decode() {
        let body = json!({
            "output":[{"type":"image_generation_call","result":"AP_="}],
            "usage":{"input_tokens":7,"output_tokens":1},
        })
        .to_string()
        .into_bytes();
        let (provider, server) = serve(body, None, None).await;
        let (dir, session) = generation_session(provider);
        let path = dir.path().join("bad.png");
        let error = generate_with(
            &session,
            &CancellationToken::new(),
            &path,
            Limits::new(u64::MAX),
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert_eq!(
            format!("{error:#}"),
            "media output is not valid base64: Invalid symbol 95, offset 2."
        );
        let records = session.usage_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tokens.input_tokens, Some(7));
        assert!(!path.exists());
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ceiling_error_is_reported_even_if_the_provider_stalls() {
        let image = noise(30_000);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&image);
        let body = json!({"output":[{"type":"image_generation_call","result":encoded}]})
            .to_string()
            .into_bytes();
        let (provider, server) = serve(body.clone(), Some(body.len() / 2), None).await;
        let (dir, session) = generation_session(provider);
        let path = dir.path().join("stalled.png");
        let error = tokio::time::timeout(
            Duration::from_secs(20),
            generate_with(
                &session,
                &CancellationToken::new(),
                &path,
                limits(1024, 1024, 4096),
            ),
        )
        .await
        .expect("the ceiling error must not wait for the stalled provider")
        .unwrap_err()
        .to_string();
        server.abort();
        assert!(
            error.starts_with("generation response exceeds generate.maxOutputBytes (4096 bytes)"),
            "{error}"
        );
        session.shutdown().await.unwrap();
    }
}
