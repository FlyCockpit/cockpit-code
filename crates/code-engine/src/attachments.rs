//! Immutable session inputs. Only references, never inline data, enter the journal.
use crate::{
    Session,
    schema::{Attachment, EngineError, HistoryEvent, MediaInput},
};
use base64::Engine as _;
use code_config::{Capabilities, Modality, ProviderType};
use serde_json::{Value, json};
use std::path::{Component, Path};

pub const MAX_BYTES: u64 = 20 * 1024 * 1024;
type Result<T> = std::result::Result<T, EngineError>;

pub fn mime_type(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "txt" | "md" | "rs" | "py" | "csv" | "log" => "text/plain",
        "json" => "application/json",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}
/// MIME media class; None denotes a generic file. UTF-8 is handled by content.
pub fn modality(attachment: &Attachment) -> Option<Modality> {
    mime_modality(&attachment.mime_type)
}
fn mime_modality(mime: &str) -> Option<Modality> {
    if mime.starts_with("image/") {
        Some(Modality::Image)
    } else if mime == "application/pdf" {
        Some(Modality::Pdf)
    } else if mime.starts_with("audio/") {
        Some(Modality::Audio)
    } else if mime.starts_with("video/") {
        Some(Modality::Video)
    } else {
        None
    }
}
fn content_modality(attachment: &Attachment, bytes: &[u8]) -> Option<Modality> {
    if let Some(mime) = sniff_media(bytes) {
        mime_modality(mime)
    } else if std::str::from_utf8(bytes).is_ok() {
        Some(Modality::Text)
    } else {
        mime_modality(sniff_media(bytes).unwrap_or(&attachment.mime_type))
    }
}
pub(crate) fn sniff_media(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        Some("audio/wav")
    } else if bytes.starts_with(b"ID3") || is_mp3_frame(bytes) {
        Some("audio/mpeg")
    } else if bytes.starts_with(b"fLaC") {
        Some("audio/flac")
    } else if bytes.starts_with(b"OggS") {
        Some("audio/ogg")
    } else if bytes.get(4..8) == Some(b"ftyp") {
        if matches!(bytes.get(8..12), Some(b"M4A " | b"M4B " | b"M4P ")) {
            Some("audio/mp4")
        } else {
            Some("video/mp4")
        }
    } else if bytes.starts_with(b"\x1a\x45\xdf\xa3") {
        Some("video/webm")
    } else {
        None
    }
}
fn is_mp3_frame(bytes: &[u8]) -> bool {
    // Require a Layer III frame with a valid MPEG version, bitrate and sample
    // rate. A sync prefix alone also matches unrelated binary data (e.g. FF FE).
    bytes.len() >= 4
        && bytes[0] == 0xff
        && bytes[1] & 0xe6 == 0xe2
        && bytes[1] & 0x18 != 0x08
        && matches!(bytes[2] >> 4, 1..=14)
        && bytes[2] & 0x0c != 0x0c
}
pub(crate) fn read(root: Option<&Path>, attachment: &Attachment) -> Result<Vec<u8>> {
    let mut parts = Path::new(&attachment.path).components();
    if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
        return Err(EngineError::bad(
            "attachment path must be a filename relative to session assets/",
        ));
    }
    let path = root
        .ok_or_else(|| EngineError::bad("attachment has no session assets directory"))?
        .join(&attachment.path);
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| {
        EngineError::bad(format!(
            "attachment {} is missing from session assets/",
            attachment.name
        ))
    })?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(EngineError::bad(
            "attachment must be a regular file of at most 20 MiB",
        ));
    }
    std::fs::read(path).map_err(|_| EngineError::bad("could not read session attachment"))
}
pub(crate) fn validate(
    capabilities: &Capabilities,
    history: &[HistoryEvent],
    root: Option<&Path>,
) -> Result<()> {
    for event in history {
        let inputs = match event {
            HistoryEvent::User { attachments, .. } => attachments
                .iter()
                .cloned()
                .map(|attachment| MediaInput::Attachment { attachment })
                .collect::<Vec<_>>(),
            HistoryEvent::ToolResult { details, .. } => details.media.clone(),
            HistoryEvent::ToolMedia { inputs, .. } => inputs.clone(),
            _ => vec![],
        };
        for input in inputs {
            let (kind, name) = match input {
                MediaInput::Attachment { attachment } => (
                    content_modality(&attachment, &read(root, &attachment)?),
                    attachment.name,
                ),
                MediaInput::Url { mime_type, .. } => {
                    (mime_modality(&mime_type), "remote media".into())
                }
            };
            let accepted = match &kind {
                Some(Modality::Text) => true,
                Some(kind) => capabilities.input.contains(kind),
                None => capabilities.file_input,
            };
            if !accepted {
                return Err(EngineError::bad(format!(
                    "model capabilities.input does not accept {:?} attachment '{}'",
                    kind, name
                )));
            }
        }
    }
    Ok(())
}
pub(crate) fn blocks(
    wire: ProviderType,
    root: Option<&Path>,
    attachments: &[Attachment],
) -> Result<Vec<Value>> {
    let mut blocks = Vec::new();
    for attachment in attachments {
        let bytes = read(root, attachment)?;
        if sniff_media(&bytes).is_none()
            && let Ok(text) = std::str::from_utf8(&bytes)
        {
            blocks.push(json!({"type":if wire == ProviderType::OpenaiResponses {"input_text"} else {"text"},"text":format!("File: {}\n{text}", attachment.name)}));
            continue;
        }
        let mime = sniff_media(&bytes).unwrap_or(&attachment.mime_type);
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let url = format!("data:{mime};base64,{data}");
        let block = match (wire, mime_modality(mime)) {
            (ProviderType::Anthropic, Some(Modality::Image)) => {
                json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}})
            }
            (ProviderType::Anthropic, Some(Modality::Pdf)) => {
                json!({"type":"document","title":attachment.name,"source":{"type":"base64","media_type":"application/pdf","data":data}})
            }
            (ProviderType::OpenaiResponses, Some(Modality::Image)) => {
                json!({"type":"input_image","image_url":url})
            }
            (ProviderType::OpenaiResponses, Some(Modality::Pdf) | None) => {
                json!({"type":"input_file","filename":attachment.name,"file_data":url})
            }
            (ProviderType::OpenaiChat, Some(Modality::Image)) => {
                json!({"type":"image_url","image_url":{"url":url}})
            }
            (ProviderType::OpenaiChat, Some(Modality::Pdf) | None) => {
                json!({"type":"file","file":{"filename":attachment.name,"file_data":url}})
            }
            (ProviderType::OpenaiChat, Some(Modality::Audio))
                if matches!(mime, "audio/wav" | "audio/mpeg") =>
            {
                json!({"type":"input_audio","input_audio":{"data":data,"format":if mime == "audio/wav" {"wav"} else {"mp3"}}})
            }
            (ProviderType::OpenaiChat, Some(Modality::Video)) => {
                json!({"type":"video_url","video_url":{"url":url}})
            }
            (ProviderType::OpenaiResponses, Some(Modality::Video)) => {
                json!({"type":"input_video","video_url":url})
            }
            _ => {
                return Err(EngineError::bad(format!(
                    "{wire:?} cannot serialize {} attachment '{}'",
                    attachment.mime_type, attachment.name
                )));
            }
        };
        blocks.push(block);
    }
    Ok(blocks)
}
impl Session {
    /// Lift legacy composer tags only at the frontend input boundary. Pasted
    /// text is opaque, and recorded/model/tool messages never pass through here.
    pub(crate) fn import_attachment_tags(
        &self,
        text: String,
        mut attachments: Vec<Attachment>,
    ) -> Result<(String, Vec<Attachment>)> {
        let mut output = String::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find('<') {
            output.push_str(&rest[..start]);
            rest = &rest[start..];
            let tag = ["paste", "attachment"].into_iter().find_map(|kind| {
                let after = rest.strip_prefix(&format!("<{kind} id=\""))?;
                let (id, tail) = after.split_once('"')?;
                if id.is_empty()
                    || id.len() > 64
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return None;
                }
                Some((kind, id, tail))
            });
            if let Some((tag, id, tail)) = tag {
                let body = if tag == "paste" {
                    tail.strip_prefix('>')
                } else {
                    tail.strip_prefix(" kind=\"").and_then(|tail| {
                        let (kind, body) = tail.split_once("\">")?;
                        matches!(kind, "text" | "image" | "pdf" | "audio" | "video" | "file")
                            .then_some(body)
                    })
                };
                let close = format!("</{tag} id=\"{id}\">");
                if let Some(body) = body
                    && let Some(end) = body.find(&close)
                {
                    let consumed = rest.len() - body.len() + end + close.len();
                    if tag == "paste" {
                        output.push_str(&rest[..consumed]);
                    } else {
                        // The declared kind is a UI hint, never a capability override.
                        let path = Path::new(&body[..end]);
                        let path = if path.is_absolute() {
                            path.to_owned()
                        } else {
                            self.cwd().join(path)
                        };
                        attachments.push(self.attach_file(path)?);
                    }
                    rest = &rest[consumed..];
                    continue;
                }
            }
            output.push('<');
            rest = &rest[1..];
        }
        output.push_str(rest);
        Ok((output, attachments))
    }
    pub fn attachment_bytes(&self, attachment: &Attachment) -> Result<Vec<u8>> {
        read(
            Some(&self.control.journal.lock().unwrap().dir.join("assets")),
            attachment,
        )
    }
    /// Copy a selected local file before recording its immutable reference.
    pub fn attach_file(&self, path: impl AsRef<Path>) -> Result<Attachment> {
        let path = path.as_ref();
        let metadata = std::fs::metadata(path)
            .map_err(|_| EngineError::bad("could not read attachment file"))?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(EngineError::bad(
                "attachment must be a regular file of at most 20 MiB",
            ));
        }
        let bytes =
            std::fs::read(path).map_err(|_| EngineError::bad("could not read attachment file"))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment.txt");
        self.attach_bytes(name, mime_type(name), &bytes)
    }
    /// Import clipboard/editor bytes. UTF-8 is inlined; binary MIME types
    /// determine capabilities.input gating, with `file` for other binaries.
    pub fn attach_bytes(&self, name: &str, mime_type: &str, bytes: &[u8]) -> Result<Attachment> {
        store(&self.control, name, mime_type, bytes)
    }
}
pub(crate) fn store(
    control: &crate::Control,
    name: &str,
    mime_type: &str,
    bytes: &[u8],
) -> Result<Attachment> {
    if bytes.len() as u64 > MAX_BYTES || bytes.is_empty() {
        return Err(EngineError::bad("attachment must contain 1 byte to 20 MiB"));
    }
    if name.contains(['\n', '\r', '\0']) || mime_type.contains(['\n', '\r', '\0']) {
        return Err(EngineError::bad("invalid attachment metadata"));
    }
    let mime_type = sniff_media(bytes).unwrap_or(mime_type);
    let name = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment")
        .to_owned();
    let extension = Path::new(&name)
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| s.len() <= 12 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    let path = format!("{}.{}", uuid::Uuid::new_v4(), extension);
    let attachment = Attachment {
        path,
        name,
        mime_type: mime_type.into(),
    };
    let mut journal = control.journal.lock().unwrap();
    let dest = journal.dir.join("assets").join(&attachment.path);
    journal
        .with_owner(|| {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(dest)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })
        .map_err(|_| EngineError::bad("could not store session attachment"))?;
    Ok(attachment)
}

pub(crate) fn media_blocks(
    wire: ProviderType,
    root: Option<&Path>,
    media: &[MediaInput],
) -> Result<Vec<Value>> {
    let mut out = vec![];
    for input in media {
        match input {
            MediaInput::Attachment { attachment } => {
                out.extend(blocks(wire, root, std::slice::from_ref(attachment))?)
            }
            MediaInput::Url { url, mime_type } => {
                let block = match (wire, mime_modality(mime_type)) {
                    (ProviderType::OpenaiChat, Some(Modality::Video)) => {
                        json!({"type":"video_url","video_url":{"url":url}})
                    }
                    (ProviderType::OpenaiResponses, Some(Modality::Video)) => {
                        json!({"type":"input_video","video_url":url})
                    }
                    _ => {
                        return Err(EngineError::bad(format!(
                            "{wire:?} has no native video input format; no frames were substituted"
                        )));
                    }
                };
                out.push(block);
            }
        }
    }
    Ok(out)
}

/// Keep a batch's native tool results contiguous. Media becomes a linked user
/// input only after all results, before the next conversational message.
pub(crate) fn expand_tool_media(history: &[HistoryEvent]) -> Vec<HistoryEvent> {
    let mut out = vec![];
    let mut pending = vec![];
    for event in history {
        if matches!(
            event,
            HistoryEvent::User { .. }
                | HistoryEvent::Assistant { .. }
                | HistoryEvent::SystemReminder { .. }
                | HistoryEvent::Compaction { .. }
        ) {
            out.append(&mut pending);
        }
        if let HistoryEvent::ToolResult { id, details, .. } = event
            && !details.media.is_empty()
        {
            pending.push(HistoryEvent::ToolMedia {
                id: id.clone(),
                inputs: details.media.clone(),
            });
        }
        out.push(event.clone());
    }
    out.append(&mut pending);
    out
}
