//! Clipboard delivery, adapted from FlyCockpit's Cockpit (sealed
//! values, redaction, generations and the daemon route left out).
//!
//! Every copy goes out through all routes that can reach a clipboard (D130):
//! native (`arboard`, local only, HTML plus a plain-text alternative, or an
//! executable — `pbcopy`, `wl-copy`, `xclip`/`xsel` — when native fails),
//! **always** OSC 52 written to the tty (tmux-aware, see [`build_sequence`]),
//! and tmux's paste buffer inside tmux. Inside tmux the TUI cannot tell
//! whether the attached client is local or remote (SSH variables come from
//! the tmux server's environment), so a native-only copy can land on the
//! wrong machine; OSC 52 reaches the terminal the user is looking at.
//! Over SSH the native and executable routes are skipped.
use std::io::Write;
use std::process::{Command, Stdio};

/// Total OSC 52 sequence cap (Cockpit's `OSC52_MAX_SEQUENCE_BYTES`).
const OSC52_MAX_SEQUENCE_BYTES: usize = 102_400;

/// How a copy reached the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Confirmed by a local route (native clipboard or an executable); the
    /// text names every route used (`clipboard + OSC 52 + tmux buffer`).
    Copied(String),
    /// OSC 52 was written (terminals send no acknowledgement); the text
    /// names the routes (`OSC 52` or `OSC 52 + tmux buffer`).
    Sent(String),
    Failed(String),
}

pub struct Delivery {
    pub outcome: Outcome,
    /// Rich text was asked for but only plain text could be delivered.
    pub downgraded: bool,
}

pub struct Clipboard {
    pub ssh: bool,
    pub tmux: bool,
    /// The one-time tmux setup hint was shown.
    pub tmux_hinted: bool,
}

pub const TMUX_HINT: &str = "if paste is empty: `set -g set-clipboard on` in tmux (or `allow-passthrough on`) and enable clipboard access in your local terminal";

impl Clipboard {
    pub fn detect() -> Self {
        let set = |v: &str| std::env::var_os(v).is_some_and(|s| !s.is_empty());
        Self {
            ssh: set("SSH_TTY") || set("SSH_CONNECTION"),
            tmux: set("TMUX"),
            tmux_hinted: false,
        }
    }

    /// Whether HTML can reach the local clipboard (only the native route carries it).
    pub fn rich_possible(&self) -> bool {
        !self.ssh && !cfg!(target_os = "linux")
    }

    /// Copy plain text.
    pub fn copy_plain(&mut self, text: &str) -> Delivery {
        Delivery {
            outcome: self.plain_chain(text),
            downgraded: false,
        }
    }

    /// Copy HTML with a plain-text alternative. Where HTML cannot reach the
    /// clipboard, `fallback` (the best plain form) is copied instead.
    pub fn copy_rich(&mut self, plain: &str, html: &str, fallback: &str) -> Delivery {
        if self.rich_possible() {
            match native_rich(plain, html) {
                Ok(()) => {
                    // The terminal gets the plain form (OSC 52 has no HTML).
                    let mut routes = vec!["clipboard"];
                    if self.osc52(plain).is_ok() {
                        routes.push("OSC 52");
                    }
                    if self.mirror(plain) {
                        routes.push("tmux buffer");
                    }
                    return Delivery {
                        outcome: Outcome::Copied(routes.join(" + ")),
                        downgraded: false,
                    };
                }
                Err(_) => {
                    return Delivery {
                        outcome: self.plain_chain(fallback),
                        downgraded: true,
                    };
                }
            }
        }
        Delivery {
            outcome: self.plain_chain(fallback),
            downgraded: true,
        }
    }

    fn plain_chain(&mut self, text: &str) -> Outcome {
        if text.is_empty() {
            return Outcome::Failed("nothing to copy".into());
        }
        let mut confirmed: Option<&'static str> = None;
        // Locally: the native clipboard, else a confirming executable. Over
        // SSH both would only reach the remote host's clipboard.
        if !self.ssh {
            confirmed = if native_plain(text).is_ok() {
                Some("clipboard")
            } else {
                executable(text)
            };
        }
        // Always OSC 52 too: inside tmux the client may be on another machine.
        let osc = self.osc52(text);
        let mirrored = self.mirror(text);
        let mut routes: Vec<&str> = confirmed.into_iter().collect();
        if osc.is_ok() {
            routes.push("OSC 52");
        }
        if mirrored {
            routes.push("tmux buffer");
        }
        match (confirmed, osc) {
            (Some(_), _) => Outcome::Copied(routes.join(" + ")),
            (None, Ok(())) => Outcome::Sent(routes.join(" + ")),
            (None, Err(e)) if mirrored => Outcome::Failed(format!("{e}; only in the tmux buffer")),
            (None, Err(e)) => Outcome::Failed(e),
        }
    }

    fn osc52(&self, text: &str) -> Result<(), String> {
        let encoded = base64(text.as_bytes());
        let seq = build_sequence(&encoded, self.tmux);
        if 8 + encoded.len() > OSC52_MAX_SEQUENCE_BYTES {
            return Err("too large for the terminal clipboard (OSC 52)".into());
        }
        let mut out = std::io::stdout();
        out.write_all(seq.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|e| e.to_string())
    }

    /// Best effort: put the text in tmux's paste buffer too.
    fn mirror(&self, text: &str) -> bool {
        self.tmux && pipe("tmux", &["load-buffer", "-"], text).is_ok()
    }
}

/// One OSC 52 frame. Inside tmux, the raw frame (which tmux itself handles
/// with `set-clipboard on`, no passthrough needed) followed by a DCS
/// passthrough frame with the inner ESCs doubled (Cockpit's
/// `build_sequence`, for `allow-passthrough on`). A terminal that gets both
/// sets the clipboard twice to the same text.
pub fn build_sequence(encoded_b64: &str, tmux: bool) -> String {
    let raw = format!("\x1b]52;c;{encoded_b64}\x07");
    if tmux {
        let inner = raw.replace('\x1b', "\x1b\x1b");
        format!("{raw}\x1bPtmux;{inner}\x1b\\")
    } else {
        raw
    }
}

#[cfg(not(target_os = "linux"))]
fn native_plain(text: &str) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text.to_string()).map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
fn native_rich(plain: &str, html: &str) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_html(html.to_string(), Some(plain.to_string()))
        .map_err(|e| e.to_string())
}

// Cockpit skips native on Linux: arboard cannot use the session's held
// Wayland/X11 connection, so the executable route covers it.
#[cfg(target_os = "linux")]
fn native_plain(_: &str) -> Result<(), String> {
    Err("native clipboard not used on Linux".into())
}

#[cfg(target_os = "linux")]
fn native_rich(_: &str, _: &str) -> Result<(), String> {
    Err("native clipboard not used on Linux".into())
}

fn executable(text: &str) -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        return pipe("/usr/bin/pbcopy", &[], text).ok().map(|()| "pbcopy");
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && pipe("wl-copy", &[], text).is_ok() {
        return Some("wl-copy");
    }
    if std::env::var_os("DISPLAY").is_some() {
        if pipe("xclip", &["-selection", "clipboard"], text).is_ok() {
            return Some("xclip");
        }
        if pipe("xsel", &["--clipboard", "--input"], text).is_ok() {
            return Some("xsel");
        }
    }
    None
}

fn pipe(program: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed"))
    }
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
