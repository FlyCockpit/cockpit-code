//! Paste blocks, adapted from FlyCockpit's Cockpit: a large paste becomes
//! one atomic chip in the composer. The chip's label lives in the buffer
//! text and its byte range is tracked beside it (see `composer::TextBuf`);
//! the full text is sent as `<paste id="R">…</paste id="R">` with a fresh
//! random id per paste, so pasted text can't forge the closing tag. User
//! messages show the chip instead of the text, and replay re-collapses
//! chips by parsing the stored tags.
//!
//! Attachment chips (D79) ride the same mechanism:
//! `<attachment id="R" kind="image">/abs/path</attachment id="R">`.
use std::io::Read;

/// A paste collapses at `pasteCollapseLines` lines (default 8) or more
/// than this many characters.
pub const COLLAPSE_CHARS: usize = 800;

const OPEN: &str = "<paste id=\"";
const AOPEN: &str = "<attachment id=\"";

/// One paste block in a composer buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// Byte range of the chip label in the buffer.
    pub start: usize,
    pub end: usize,
    /// The pasted text, sent verbatim.
    pub full: String,
    /// The random tag id used on the wire.
    pub id: String,
    /// Display number (`paste #n`, `image #n`), one-based and never reused
    /// in a buffer (pastes and attachments count separately).
    pub n: u32,
    /// An attachment chip (D79): its kind (image, pdf, audio, video, file);
    /// `full` is then the absolute path.
    pub attach: Option<String>,
    /// The active model can't take this attachment's input type.
    pub warn: bool,
}

impl Block {
    /// The chip's wire text.
    pub fn wire(&self) -> String {
        match &self.attach {
            Some(kind) => attach_wire(&self.id, kind, &self.full),
            None => wire(&self.id, &self.full),
        }
    }
}

pub fn should_collapse(text: &str, lines: usize) -> bool {
    text.lines().count() >= lines.max(1) || text.chars().count() > COLLAPSE_CHARS
}

fn fmt_chars(n: usize) -> String {
    if n < 1000 {
        format!("{n} chars")
    } else {
        format!("{:.1}k chars", n as f64 / 1000.0)
    }
}

/// `[paste #1 · 120 lines · 4.1k chars]`
pub fn label(n: u32, full: &str) -> String {
    let lines = full.lines().count().max(1);
    format!(
        "[paste #{n} · {lines} line{} · {}]",
        if lines == 1 { "" } else { "s" },
        fmt_chars(full.chars().count())
    )
}

fn close_tag(id: &str) -> String {
    format!("</paste id=\"{id}\">")
}

/// 80 random bits as 16 base32 characters (from the OS; a hashed clock as a
/// fallback), never colliding with a closing tag inside `full`.
pub fn fresh_id(full: &str) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    loop {
        let mut bytes = [0u8; 10];
        let ok = std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .is_ok();
        if !ok {
            use std::hash::{BuildHasher, Hasher};
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos()),
            );
            bytes[..8].copy_from_slice(&h.finish().to_le_bytes());
        }
        let mut bits: u128 = 0;
        for b in bytes {
            bits = (bits << 8) | b as u128;
        }
        let id: String = (0..16)
            .map(|i| ALPHABET[((bits >> (i * 5)) & 31) as usize] as char)
            .collect();
        if !full.contains(&close_tag(&id)) {
            return id;
        }
    }
}

pub fn wire(id: &str, full: &str) -> String {
    format!("<paste id=\"{id}\">{full}{}", close_tag(id))
}

fn attach_close(id: &str) -> String {
    format!("</attachment id=\"{id}\">")
}

pub fn attach_wire(id: &str, kind: &str, path: &str) -> String {
    format!(
        "<attachment id=\"{id}\" kind=\"{kind}\">{path}{}",
        attach_close(id)
    )
}

/// A piece of a wire message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg<'a> {
    Text(&'a str),
    Paste {
        id: &'a str,
        body: &'a str,
    },
    Attach {
        id: &'a str,
        kind: &'a str,
        path: &'a str,
    },
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// An attachment tag at the start of `s`: (id, kind, path, length).
fn parse_attach(s: &str) -> Option<(&str, &str, &str, usize)> {
    let after = s.strip_prefix(AOPEN)?;
    let q = after.find('"')?;
    let id = &after[..q];
    if !valid_id(id) {
        return None;
    }
    let rest = after[q..].strip_prefix("\" kind=\"")?;
    let k = rest.find("\">")?;
    let kind = &rest[..k];
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    let body = &rest[k + 2..];
    let close = attach_close(id);
    let e = body.find(&close)?;
    let path = &body[..e];
    let len = s.len() - body.len() + e + close.len();
    Some((id, kind, path, len))
}

/// Split a wire message into text and paste blocks. A malformed or
/// unclosed tag stays text.
pub fn parse(text: &str) -> Vec<Seg<'_>> {
    let mut out = vec![];
    let mut rest = text;
    let mut text_start = 0usize;
    let mut pos = 0usize;
    loop {
        let p = rest.find(OPEN);
        let a = rest.find(AOPEN);
        let at = match (p, a) {
            (None, None) => break,
            (Some(p), Some(a)) => p.min(a),
            (Some(x), None) | (None, Some(x)) => x,
        };
        if Some(at) == a {
            match parse_attach(&rest[at..]) {
                Some((id, kind, path, len)) => {
                    let abs = pos + at;
                    if abs > text_start {
                        out.push(Seg::Text(&text[text_start..abs]));
                    }
                    out.push(Seg::Attach { id, kind, path });
                    text_start = abs + len;
                    pos = text_start;
                }
                None => pos += at + AOPEN.len(),
            }
            rest = &text[pos..];
            continue;
        }
        let after = &rest[at + OPEN.len()..];
        let parsed = after.find("\">").and_then(|q| {
            let id = &after[..q];
            if !valid_id(id) {
                return None;
            }
            let body_start = q + 2;
            let close = close_tag(id);
            after[body_start..]
                .find(&close)
                .map(|e| (id, body_start, body_start + e, body_start + e + close.len()))
        });
        match parsed {
            Some((id, b0, b1, end)) => {
                let abs = pos + at;
                if abs > text_start {
                    out.push(Seg::Text(&text[text_start..abs]));
                }
                let base = abs + OPEN.len();
                out.push(Seg::Paste {
                    id,
                    body: &text[base + b0..base + b1],
                });
                text_start = base + end;
                pos = text_start;
                rest = &text[pos..];
            }
            None => {
                pos += at + OPEN.len();
                rest = &text[pos..];
            }
        }
    }
    if text_start < text.len() {
        out.push(Seg::Text(&text[text_start..]));
    }
    out
}

/// The message with each paste replaced by its chip label (one line).
pub fn collapse(text: &str) -> String {
    let (mut n, mut a) = (0, 0);
    parse(text)
        .into_iter()
        .map(|s| match s {
            Seg::Text(t) => t.to_string(),
            Seg::Paste { body, .. } => {
                n += 1;
                label(n, body)
            }
            Seg::Attach { kind, path, .. } => {
                a += 1;
                crate::attach::label(a, kind, path)
            }
        })
        .collect()
}

/// The message with every paste's text inlined, without tags (copying).
pub fn expand(text: &str) -> String {
    parse(text)
        .into_iter()
        .map(|s| match s {
            Seg::Text(t) | Seg::Paste { body: t, .. } => t.to_string(),
            Seg::Attach { path, .. } => crate::attach::mention(path),
        })
        .collect()
}

/// The attachments of a message, in order: (kind, path).
pub fn attachments(text: &str) -> Vec<(String, String)> {
    parse(text)
        .into_iter()
        .filter_map(|s| match s {
            Seg::Attach { kind, path, .. } => Some((kind.to_string(), path.to_string())),
            _ => None,
        })
        .collect()
}

/// The pastes of a message, in order.
pub fn bodies(text: &str) -> Vec<String> {
    parse(text)
        .into_iter()
        .filter_map(|s| match s {
            Seg::Paste { body, .. } => Some(body.to_string()),
            Seg::Text(_) | Seg::Attach { .. } => None,
        })
        .collect()
}

/// Editor delimiters around an expanded paste (Cockpit's
/// `display_paste_boundary`).
pub fn editor_open(n: u32) -> String {
    format!("--- paste #{n} start ---")
}
pub fn editor_close(n: u32) -> String {
    format!("--- paste #{n} end ---")
}
