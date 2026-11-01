//! ACP-prescribed LF stdio codec, adapted from FlyCockpit's Cockpit.
//!
//! One UTF-8 JSON-RPC 2.0 object per physical LF-delimited line. Not LSP
//! `Content-Length` framing. The cap is the UTF-8 byte length of the JSON
//! value without its LF. Cockpit's daemon counters and test sinks are dropped.

use std::io::{self, Read};

/// Exact UTF-8 byte cap for one ACP JSON-RPC frame, LF excluded.
pub const MAX_FRAME_BYTES: usize = 2_097_152;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    InvalidUtf8,
    OverLimit { json_bytes: usize },
    IncompleteEof { json_bytes: usize },
    EmbeddedLineBreak,
    ContentLengthFraming,
    CarriageReturnFraming,
    NonJsonObject,
    BatchArray,
    Empty,
    Io(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 => f.write_str("ACP frame is not valid UTF-8"),
            Self::OverLimit { json_bytes } => write!(
                f,
                "ACP frame exceeds {MAX_FRAME_BYTES} bytes ({json_bytes})"
            ),
            Self::IncompleteEof { json_bytes } => {
                write!(f, "ACP frame ended at EOF without LF ({json_bytes} bytes)")
            }
            Self::EmbeddedLineBreak => f.write_str("ACP frame contains an embedded line break"),
            Self::ContentLengthFraming => f.write_str("ACP frame uses Content-Length/LSP framing"),
            Self::CarriageReturnFraming => {
                f.write_str("ACP frame uses forbidden carriage-return framing")
            }
            Self::NonJsonObject => f.write_str("ACP frame is not a JSON object"),
            Self::BatchArray => f.write_str("ACP v1 does not accept JSON-RPC batch arrays"),
            Self::Empty => f.write_str("ACP frame is empty"),
            Self::Io(message) => write!(f, "ACP stdio I/O error: {message}"),
        }
    }
}

/// Reads complete LF-delimited frames from a byte source.
pub struct LineReader<R> {
    inner: R,
    pending: Vec<u8>,
    closed: bool,
}

impl<R: Read> LineReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            closed: false,
        }
    }

    /// The next complete frame. `Ok(None)` is EOF; after an over-limit
    /// unterminated frame the reader is closed and returns `Ok(None)`.
    pub fn read_frame(&mut self) -> Result<Option<String>, FrameError> {
        loop {
            if let Some(frame) = self.take_complete_line()? {
                return Ok(Some(frame));
            }
            if self.closed {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                let json_bytes = self.pending.len();
                self.pending.clear();
                return Err(FrameError::IncompleteEof { json_bytes });
            }
            let mut buf = [0u8; 8192];
            match self.inner.read(&mut buf) {
                Ok(0) => self.closed = true,
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    self.closed = true;
                    self.pending.clear();
                    return Err(FrameError::Io(err.to_string()));
                }
            }
            if self.pending.len() > MAX_FRAME_BYTES && !self.pending.contains(&b'\n') {
                let json_bytes = self.pending.len();
                self.pending.clear();
                self.closed = true;
                return Err(FrameError::OverLimit { json_bytes });
            }
        }
    }

    fn take_complete_line(&mut self) -> Result<Option<String>, FrameError> {
        let Some(newline_at) = self.pending.iter().position(|b| *b == b'\n') else {
            return Ok(None);
        };
        let mut line: Vec<u8> = self.pending.drain(..=newline_at).collect();
        line.pop();
        let json = String::from_utf8(line).map_err(|_| FrameError::InvalidUtf8)?;
        if looks_like_content_length(&json) {
            return Err(FrameError::ContentLengthFraming);
        }
        if json.contains('\r') {
            return Err(FrameError::CarriageReturnFraming);
        }
        if json.len() > MAX_FRAME_BYTES {
            return Err(FrameError::OverLimit {
                json_bytes: json.len(),
            });
        }
        reject_non_object(&json)?;
        Ok(Some(json))
    }
}

/// Outbound check: one JSON-RPC 2.0 object that fits the cap with no line break.
pub fn prepare_outbound(json: &str) -> Result<(), FrameError> {
    if json.len() > MAX_FRAME_BYTES {
        return Err(FrameError::OverLimit {
            json_bytes: json.len(),
        });
    }
    if json.contains('\n') || json.contains('\r') {
        return Err(FrameError::EmbeddedLineBreak);
    }
    reject_non_object(json)
}

fn reject_non_object(json: &str) -> Result<(), FrameError> {
    let trimmed = json.trim_start_matches([' ', '\t']);
    if trimmed.is_empty() {
        return Err(FrameError::Empty);
    }
    if trimmed.starts_with('[') {
        return Err(FrameError::BatchArray);
    }
    if !trimmed.starts_with('{') {
        return Err(FrameError::NonJsonObject);
    }
    Ok(())
}

fn looks_like_content_length(json: &str) -> bool {
    let lower = json
        .trim_start_matches([' ', '\t', '\r'])
        .to_ascii_lowercase();
    lower.starts_with("content-length:") || lower.starts_with("content-type:")
}
