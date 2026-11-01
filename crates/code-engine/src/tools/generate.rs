//! §8 / D14 media generation: `generate {kind, prompt, model?, inputs?, outPath?}`.
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
//! - `command`: uses the M7 transport with reference attachments and binary outputFile.
use super::{PermissionGate, PreparedOperation, ToolDefinition};
use crate::{paths::Resolver, schema::Tokens, schema::UsageRecord};
use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use code_config::{
    ImageRequest, Modality, ModelConfig, Provider, ProviderType, ReportedCost, Snapshot,
};
use serde_json::{Map, Value, json};
use std::{
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

/// Whether `generate` belongs in a new agent's `tools[]` (fixed afterwards, D61).
pub(crate) fn offered(snapshot: &Snapshot) -> bool {
    snapshot.providers.values().any(|p| {
        p.enabled
            && p.models
                .values()
                .any(|m| m.capabilities.output.iter().any(media))
    })
}

pub(crate) struct Context<'a> {
    pub control: &'a crate::Control,
    pub gate: &'a dyn PermissionGate,
    pub op: PreparedOperation,
    pub snapshot: Snapshot,
    pub session_id: String,
    pub agent_id: String,
    pub assets: PathBuf,
    pub cancel: &'a CancellationToken,
}

pub(crate) struct Generated {
    pub output: String,
    pub usage: Option<UsageRecord>,
}

pub(crate) async fn execute(mut ctx: Context<'_>) -> Result<Generated> {
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
    let (media, usage) = match kind_wire {
        ProviderType::OpenaiResponses | ProviderType::OpenaiChat => {
            let request = http_request(kind_wire, &model, &model_id, kind_name, &prompt, &inputs)?;
            tokio::select! {
                _ = ctx.cancel.cancelled() => bail!("cancelled"),
                r = tokio::time::timeout(TIMEOUT, send(kind_wire, &provider, &model, request)) =>
                    r.map_err(|_| anyhow::anyhow!("generation timed out after {}s", TIMEOUT.as_secs()))??,
            }
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
            )
            .await;
        }
        ProviderType::Anthropic => {
            bail!(
                "{full_id}: the anthropic wire has no media output; use openai-responses, openai-chat or a command provider"
            )
        }
    };
    let Some((bytes, mime)) = media else {
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
    write_new(&path, &bytes)?;
    let usage = usage.map(|(tokens, cost)| UsageRecord {
        call_id: ctx.op.call.id.clone(),
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
    });
    Ok(Generated {
        output: json!({"path": path, "model": full_id, "mimeType": mime, "bytes": bytes.len()})
            .to_string(),
        usage,
    })
}

fn provider_kind(provider: &Provider, model: &ModelConfig) -> ProviderType {
    model.kind.unwrap_or(provider.kind)
}

fn choose(
    snapshot: &Snapshot,
    requested: Option<&str>,
    kind: &Modality,
    kind_name: &str,
) -> Result<(String, String, Provider, ModelConfig)> {
    let candidates = snapshot
        .providers
        .iter()
        .filter(|(_, p)| p.enabled)
        .flat_map(|(pid, p)| {
            p.models
                .iter()
                .filter(|(_, m)| m.capabilities.output.contains(kind))
                .map(move |(mid, m)| (pid.clone(), mid.clone(), p.clone(), m.clone()))
        })
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

fn data_url(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    if meta.len() > MAX_INPUT {
        bail!("{} is larger than 20 MiB", path.display());
    }
    let bytes = std::fs::read(path)?;
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

async fn send(
    wire: ProviderType,
    provider: &Provider,
    model: &ModelConfig,
    body: Value,
) -> Result<(Option<(Vec<u8>, String)>, Usage)> {
    let base = provider
        .connection_url()
        .context("provider has no baseUrl")?
        .trim_end_matches('/');
    let suffix = if wire == ProviderType::OpenaiResponses {
        "/responses"
    } else {
        "/chat/completions"
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .build()?;
    let mut request = client.post(format!("{base}{suffix}")).json(&body);
    for (k, v) in provider.connection_headers() {
        request = request.header(k.as_str(), v.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("provider network error"))?;
    let status = response.status();
    let header_cost = match model
        .reported_cost
        .as_ref()
        .or(provider.reported_cost.as_ref())
    {
        Some(ReportedCost::Header { name, unit }) => response
            .headers()
            .get(name)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<f64>().ok())
            .and_then(|v| unit.dollars(v)),
        _ => None,
    };
    let text = response
        .text()
        .await
        .map_err(|_| anyhow::anyhow!("provider network error"))?;
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() || value.get("error").is_some_and(|e| !e.is_null()) {
        let message = value["error"]["message"]
            .as_str()
            .or_else(|| value["error"].as_str())
            .unwrap_or("request failed")
            .chars()
            .take(400)
            .collect::<String>();
        bail!("generation failed (HTTP {}): {message}", status.as_u16());
    }
    let media = if wire == ProviderType::OpenaiResponses {
        responses_media(&value)
    } else {
        chat_media(&value, &body)
    }?;
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
    let body_cost = match model
        .reported_cost
        .as_ref()
        .or(provider.reported_cost.as_ref())
    {
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
    Ok((media, Some((tokens, header_cost.or(body_cost).or(rated)))))
}

/// `(bytes, mime)` from a base64 string or a `data:<mime>;base64,` URL.
fn decode(raw: &str, default_mime: &str) -> Result<(Vec<u8>, String)> {
    let (declared, data) = match raw.strip_prefix("data:") {
        Some(rest) => {
            let (meta, data) = rest.split_once(',').context("malformed data URL")?;
            (meta.split(';').next().filter(|m| !m.is_empty()), data)
        }
        None => (None, raw),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .context("media output is not valid base64")?;
    if bytes.is_empty() {
        bail!("media output is empty");
    }
    let mime = declared
        .map(str::to_owned)
        .or_else(|| sniff(&bytes).map(str::to_owned))
        .unwrap_or_else(|| default_mime.to_owned());
    Ok((bytes, mime))
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

fn responses_media(value: &Value) -> Result<Option<(Vec<u8>, String)>> {
    for item in value["output"].as_array().into_iter().flatten() {
        if item["type"] == "image_generation_call"
            && let Some(result) = item["result"].as_str()
        {
            return decode(result, "image/png").map(Some);
        }
        for part in item["content"].as_array().into_iter().flatten() {
            if let Some(url) = part["image_url"]
                .as_str()
                .filter(|u| u.starts_with("data:"))
            {
                return decode(url, "image/png").map(Some);
            }
        }
    }
    Ok(None)
}

fn chat_media(value: &Value, body: &Value) -> Result<Option<(Vec<u8>, String)>> {
    let message = &value["choices"][0]["message"];
    for image in message["images"].as_array().into_iter().flatten() {
        if let Some(url) = image["image_url"]["url"].as_str() {
            return decode(url, "image/png").map(Some);
        }
    }
    if let Some(data) = message["audio"]["data"].as_str() {
        let format = body["audio"]["format"].as_str().unwrap_or("wav");
        let mime = match format {
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "opus" => "audio/opus",
            "aac" => "audio/aac",
            _ => "audio/wav",
        };
        return decode(data, mime).map(Some);
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

/// Atomic create: never replaces an existing file.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temp = super::new_file_temp(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .map_err(|e| anyhow::anyhow!("{}: {}", path.display(), e.error))?;
    Ok(())
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
) -> Result<Generated> {
    let dir = tempfile::tempdir()?;
    let assets = dir.path().join("assets");
    std::fs::create_dir(&assets)?;
    let mut attachments = Vec::new();
    for path in inputs {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("invalid reference filename")?;
        let bytes = std::fs::read(path)?;
        if bytes.len() as u64 > MAX_INPUT {
            bail!("reference input exceeds 20 MiB");
        }
        let stored = format!("{}-{}", uuid::Uuid::new_v4(), name);
        std::fs::write(assets.join(&stored), bytes)?;
        attachments.push(crate::schema::Attachment {
            path: stored,
            name: name.into(),
            mime_type: crate::attachments::mime_type(name).into(),
        });
    }
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
    let bytes = std::fs::read(output)?;
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
    write_new(&path, &bytes)?;
    Ok(Generated {
        output: json!({"path":path,"model":model.id(),"mimeType":mime,"bytes":bytes.len()})
            .to_string(),
        usage: None,
    })
}
