//! Capability-gated model inputs. Bytes are immutable assets, never text/base64 tool output.
use crate::{
    State, attachments,
    schema::*,
    tools::{CommitBarrier, ToolDefinition, ToolOutput},
};
use anyhow::{Context, Result, bail};
use code_config::{Capabilities, Modality, ProviderType};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{io::Cursor, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "read_image".into(),
        description: "Inspect an available image when the answer depends on its pixels; crop/downscale to the relevant region. Not source-code/text reading or proof of UI behavior that is not visible. Supply exactly one path, http(s) URL or attachment name/ID. crop uses pixel x/y/width/height; max_width/max_height default 1568. Returns image input.".into(),
        schema: json!({"type":"object","properties":{
            "path":{"type":"string","x-cockpit-kind":"path","x-cockpit-aliases":["file","file_path"]},"url":{"type":"string"},"attachment":{"type":"string"},
            "crop":{"type":"object","properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}},"required":["x","y","width","height"],"additionalProperties":false},
            "max_width":{"type":"integer","minimum":1,"maximum":4096},"max_height":{"type":"integer","minimum":1,"maximum":4096}
        },"additionalProperties":false}),
    }, ToolDefinition {
        name: "read_video".into(),
        description: "View a video from exactly one path or http(s) URL using the provider's native video input. Unsupported providers return an error; no frame extraction or silent conversion.".into(),
        schema: json!({"type":"object","properties":{"path":{"type":"string","x-cockpit-kind":"path","x-cockpit-aliases":["file","file_path"]},"url":{"type":"string"}},"additionalProperties":false}),
    }]
}
pub(crate) fn handles(name: &str) -> bool {
    matches!(name, "read_image" | "read_video")
}
pub(crate) fn filter(definitions: &mut Vec<ToolDefinition>, capabilities: &Capabilities) {
    definitions.retain(|d| match d.name.as_str() {
        "read_image" => capabilities.input.contains(&Modality::Image),
        "read_video" => capabilities.input.contains(&Modality::Video),
        _ => true,
    });
}
pub(crate) async fn execute(
    state: &State,
    mut call: ToolCall,
    cancel: CancellationToken,
    barrier: Arc<CommitBarrier>,
) -> ToolOutput {
    let result = run(state, &mut call, &cancel, barrier).await;
    match result {
        Ok((output, media)) => ToolOutput {
            output,
            details: ToolResultDetails {
                media: vec![media],
                ..Default::default()
            },
            is_error: false,
            canonical_call: call,
        },
        Err(e) => ToolOutput {
            output: format!("Error: {e:#}"),
            details: Default::default(),
            is_error: true,
            canonical_call: call,
        },
    }
}
async fn run(
    state: &State,
    call: &mut ToolCall,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
) -> Result<(String, MediaInput)> {
    state.control.check_owner()?;
    let model = state.model()?;
    let video = call.name == "read_video";
    let kind = if video {
        Modality::Video
    } else {
        Modality::Image
    };
    if !model.config.capabilities.input.contains(&kind) {
        bail!("the active model does not accept {kind:?} input");
    }
    if video
        && !matches!(
            model.provider.kind,
            ProviderType::OpenaiChat | ProviderType::OpenaiResponses
        )
    {
        bail!("this provider has no native video input format; no frames were substituted");
    }
    if state
        .job_id
        .as_ref()
        .and_then(|id| state.control.job_record(id).ok())
        .is_some_and(|job| job.kind == "script")
        || (state.script_depth > 0
            && state
                .tools
                .foreground
                .as_ref()
                .is_some_and(|f| f.is_background()))
    {
        bail!(
            "media inputs require a foreground tool or foreground script so their payload can reach the model"
        );
    }
    let args = call.arguments.clone();
    let sources = ["path", "url", "attachment"]
        .into_iter()
        .filter(|k| args[*k].is_string())
        .collect::<Vec<_>>();
    if sources.len() != 1 || (video && sources[0] == "attachment") {
        bail!(
            "provide exactly one path or URL{}",
            if video { "" } else { " or attachment" }
        );
    }
    let assets = state.control.journal.lock().unwrap().dir.join("assets");
    let mut source_attachment = None;
    let target = if let Some(raw) = args["path"].as_str() {
        Some(state.tools.resolver.resolve(raw)?)
    } else if let Some(id) = args["attachment"].as_str() {
        let mut found = std::collections::BTreeMap::new();
        for event in state.history() {
            let attachments = match event {
                HistoryEvent::User { attachments, .. } => attachments,
                HistoryEvent::ToolResult { details, .. } => details
                    .media
                    .into_iter()
                    .filter_map(|m| match m {
                        MediaInput::Attachment { attachment } => Some(attachment),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            };
            for a in attachments {
                if a.path == id || a.name == id {
                    found.insert(a.path.clone(), a);
                }
            }
        }
        if found.len() != 1 {
            bail!(
                "attachment must identify exactly one attachment in this context; use its asset filename if names collide"
            );
        }
        let attachment = found.into_values().next().unwrap();
        let target = assets.join(&attachment.path);
        source_attachment = Some(attachment);
        Some(target)
    } else {
        None
    };
    if let Some(path) = &target
        && args["path"].is_string()
    {
        call.arguments["path"] = json!(path);
    }
    let mut op = state.metadata_operation(call.clone(), target.clone());
    if let Some(url) = args["url"].as_str() {
        let parsed = reqwest::Url::parse(url)?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("URL must use http or https");
        }
        if state
            .model_settings()
            .exclusive
            .as_ref()
            .is_some_and(|ex| ex.block_network)
        {
            bail!("media URLs are blocked by exclusive mode");
        }
        op.command = Some(url.into());
    }
    state.gate.authorize(&op, cancel).await?;
    if video && let Some(url) = args["url"].as_str() {
        return Ok((
            "Video URL attached as native video input.".into(),
            MediaInput::Url {
                url: url.into(),
                mime_type: "video/mp4".into(),
            },
        ));
    }
    let bytes = if let Some(path) = target {
        state.tools.resolver.validate(&path)?;
        let a = source_attachment;
        tokio::task::spawn_blocking(move || {
            if let Some(a) = a {
                attachments::read(Some(&assets), &a).map_err(anyhow::Error::from)
            } else {
                let bytes = super::read_bytes(&path)?;
                if bytes.len() as u64 > attachments::MAX_BYTES {
                    bail!("media exceeds the 20 MiB input limit");
                }
                Ok(bytes)
            }
        })
        .await??
    } else {
        let url = args["url"].as_str().context("missing URL")?;
        tokio::select! {
            _ = cancel.cancelled() => bail!("cancelled"),
            result = tokio::time::timeout(Duration::from_secs(45), download(url)) => result.context("image download timed out")??,
        }
    };
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    let (bytes, name, mime, dimensions) = if video {
        let mime = attachments::sniff_media(&bytes)
            .filter(|m| m.starts_with("video/"))
            .context("file is not a supported MP4/WebM video")?;
        let extension = if mime == "video/webm" { "webm" } else { "mp4" };
        (
            bytes,
            format!("video.{extension}"),
            mime.to_owned(),
            String::new(),
        )
    } else {
        let (bytes, width, height) =
            tokio::task::spawn_blocking(move || transform(&bytes, &args)).await??;
        (
            bytes,
            "image.png".into(),
            "image/png".into(),
            format!(" ({width}×{height} pixels)"),
        )
    };
    let attachment =
        barrier.commit(|| Ok(attachments::store(&state.control, &name, &mime, &bytes)?))?;
    let output = format!(
        "{} attached{dimensions}; attachment: {}",
        if video { "Video" } else { "Image" },
        attachment.path
    );
    Ok((output, MediaInput::Attachment { attachment }))
}
async fn download(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(url)
        .send()
        .await?;
    if !response.status().is_success() {
        bail!(
            "image URL returned HTTP {}; use its direct URL",
            response.status()
        );
    }
    if response
        .content_length()
        .is_some_and(|n| n > attachments::MAX_BYTES)
    {
        bail!("image exceeds 20 MiB");
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > attachments::MAX_BYTES as usize {
            bail!("image exceeds 20 MiB");
        }
        bytes.extend(chunk);
    }
    Ok(bytes)
}
fn transform(bytes: &[u8], args: &Value) -> Result<(Vec<u8>, u32, u32)> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let mut image = reader
        .decode()
        .context("decode image (PNG/JPEG/GIF/WebP/BMP/TIFF)")?;
    if args["crop"].is_object() {
        let c = &args["crop"];
        let coordinate = |key: &str| -> Result<u32> {
            u32::try_from(
                c[key]
                    .as_u64()
                    .context("crop coordinates must be integers")?,
            )
            .context("crop coordinate is too large")
        };
        let (x, y, width, height) = (
            coordinate("x")?,
            coordinate("y")?,
            coordinate("width")?,
            coordinate("height")?,
        );
        if width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|n| n > image.width())
            || y.checked_add(height).is_none_or(|n| n > image.height())
        {
            bail!(
                "crop lies outside the {}×{} image",
                image.width(),
                image.height()
            );
        }
        image = image.crop_imm(x, y, width, height);
    }
    let width = args["max_width"].as_u64().unwrap_or(1568).clamp(1, 4096) as u32;
    let height = args["max_height"].as_u64().unwrap_or(1568).clamp(1, 4096) as u32;
    if image.width() > width || image.height() > height {
        image = image.thumbnail(width, height);
    }
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png)?;
    Ok((out.into_inner(), image.width(), image.height()))
}
