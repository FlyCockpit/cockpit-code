//! Attention signals: when a prompt starts waiting (approval, question,
//! proposal, roadblock, trust) or a turn finishes, ring the terminal bell
//! (tmux flags the window with `monitor-bell`), mark the window title
//! (`● approval waiting · …`), and with `attention: "desktop"` also send an
//! OSC 9 / OSC 777 notification. Debounced, held while the user is typing,
//! and skipped while the terminal reports that it has focus.
use super::{App, Prompt};
use std::time::{Duration, Instant};

/// Signals closer together than this collapse into one.
const DEBOUNCE: Duration = Duration::from_secs(5);
/// A keystroke this recent means the user is at the keyboard.
const TYPING: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Attention {
    /// A signal waiting to fire (reason, since).
    pending: Option<String>,
    last_fire: Option<Instant>,
    /// The title marker, until the user comes back.
    pub marker: Option<String>,
    /// Terminal focus, when the terminal reports it (focus events).
    pub focused: Option<bool>,
    /// Bytes for the event loop to write to the terminal.
    out: Option<String>,
}

impl App {
    /// Something wants the user: fire soon (see [`App::attention_tick`]).
    pub(super) fn attend(&mut self, reason: &str) {
        if self.catalog.attention == "off" {
            return;
        }
        self.attention.pending = Some(reason.to_string());
    }

    /// The reason a newly queued prompt wants the user.
    pub(super) fn prompt_reason(p: &Prompt) -> &'static str {
        match p {
            Prompt::Approval(_) => "approval waiting",
            Prompt::Question(..) => "question waiting",
            Prompt::Guidance(..) => "trust prompt waiting",
            Prompt::Scope(..) => "scope request waiting",
            Prompt::Goal(g, _) => match g {
                super::GoalPrompt::Draft(_) => "goal draft waiting",
                super::GoalPrompt::Proposal { .. } => "proposal waiting",
                super::GoalPrompt::Roadblock { .. } => "roadblock waiting",
            },
        }
    }

    /// Fire a pending signal once the user is not typing and the debounce
    /// window has passed. True when the title changed.
    pub fn attention_tick(&mut self) -> bool {
        let Some(reason) = self.attention.pending.clone() else {
            return false;
        };
        if self.attention.focused == Some(true) {
            // They are looking at it.
            self.attention.pending = None;
            return false;
        }
        if self.last_key.elapsed() < TYPING
            || self
                .attention
                .last_fire
                .is_some_and(|t| t.elapsed() < DEBOUNCE)
        {
            return false;
        }
        self.attention.pending = None;
        self.attention.last_fire = Some(Instant::now());
        let mut out = String::from("\x07");
        if self.catalog.attention == "desktop" {
            let title = self.title.clone().unwrap_or_else(|| "cockpit-code".into());
            let body = format!("{reason} · {title}").replace(['\x07', '\x1b', ';'], " ");
            // OSC 9 (iTerm2, WezTerm, Windows Terminal) and OSC 777 (urxvt,
            // foot, Ghostty); inside tmux each is wrapped for passthrough.
            let osc9 = format!("\x1b]9;{body}\x07");
            let osc777 = format!("\x1b]777;notify;cockpit-code;{body}\x07");
            for seq in [osc9, osc777] {
                if std::env::var_os("TMUX").is_some() {
                    out.push_str(&format!(
                        "\x1bPtmux;{}\x1b\\",
                        seq.replace('\x1b', "\x1b\x1b")
                    ));
                } else {
                    out.push_str(&seq);
                }
            }
        }
        self.attention.out = Some(out);
        self.attention.marker = Some(reason);
        true
    }

    /// The bytes to write to the terminal now, if a signal fired.
    pub fn take_signal(&mut self) -> Option<String> {
        self.attention.out.take()
    }

    /// The user is back (a key, a click, focus): drop the title marker.
    pub(super) fn attention_seen(&mut self) {
        self.attention.marker = None;
    }
}
