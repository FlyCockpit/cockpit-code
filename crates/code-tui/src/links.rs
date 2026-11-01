//! Links in the drawn transcript: URLs found in the visible rows become
//! OSC 8 hyperlinks (written after each frame over the same cells, so the
//! layout never sees an escape sequence) and click targets that open the
//! URL in the system browser.
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use std::io::Write;

/// One visible URL: where it is drawn and how.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub x: u16,
    pub y: u16,
    pub text: String,
    pub url: String,
    pub fg: Color,
    pub bg: Color,
    pub modifier: Modifier,
}

impl Link {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: unicode_width::UnicodeWidthStr::width(self.text.as_str()) as u16,
            height: 1,
        }
    }
}

/// URLs in the rows of `area`.
pub fn scan(buf: &Buffer, area: Rect) -> Vec<Link> {
    let mut out = vec![];
    for y in area.top()..area.bottom() {
        // The row's text with the column of each char.
        let mut chars: Vec<(char, u16)> = vec![];
        for x in area.left()..area.right() {
            let Some(cell) = buf.cell((x, y)) else {
                continue;
            };
            for c in cell.symbol().chars() {
                chars.push((c, x));
            }
        }
        let text: String = chars.iter().map(|(c, _)| *c).collect();
        let idx: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        let mut from = 0;
        while let Some(at) = ["https://", "http://"]
            .iter()
            .filter_map(|p| text[from..].find(p).map(|i| from + i))
            .min()
        {
            let start = idx.iter().position(|&b| b == at).unwrap_or(0);
            let mut end = start;
            while end < chars.len() && !stop(chars[end].0) {
                end += 1;
            }
            // Trailing punctuation belongs to the sentence; a closing
            // bracket only when the URL opened none.
            while end > start {
                let c = chars[end - 1].0;
                let url: String = chars[start..end].iter().map(|(c, _)| *c).collect();
                let unbalanced = (c == ')' && url.matches('(').count() < url.matches(')').count())
                    || (c == ']' && url.matches('[').count() < url.matches(']').count());
                if ".,;:!?'\"".contains(c) || unbalanced {
                    end -= 1;
                } else {
                    break;
                }
            }
            let url: String = chars[start..end].iter().map(|(c, _)| *c).collect();
            let byte_end = idx.get(end).copied().unwrap_or(text.len());
            from = byte_end.max(at + 1);
            if url.len() <= "https://".len() {
                continue;
            }
            let x = chars[start].1;
            let cell = buf.cell((x, y)).cloned().unwrap_or_default();
            out.push(Link {
                x,
                y,
                text: url.clone(),
                url,
                fg: cell.fg,
                bg: cell.bg,
                modifier: cell.modifier,
            });
        }
    }
    out
}

fn stop(c: char) -> bool {
    c.is_whitespace() || "<>\"`|│┊┃▌".contains(c)
}

/// Write `links` as OSC 8 hyperlinks over their cells (cursor saved and
/// restored). Inside tmux the sequences pass as-is; tmux 3.4+ forwards them.
pub fn emit(links: &[Link]) {
    if links.is_empty() {
        return;
    }
    use ratatui::backend::IntoCrossterm;
    use ratatui::crossterm::{
        cursor::{MoveTo, RestorePosition, SavePosition},
        queue,
        style::{
            Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
        },
    };
    let mut out = std::io::stdout();
    let _ = queue!(out, SavePosition);
    for l in links {
        let _ = queue!(
            out,
            MoveTo(l.x, l.y),
            SetForegroundColor(l.fg.into_crossterm()),
            SetBackgroundColor(l.bg.into_crossterm())
        );
        if l.modifier.contains(Modifier::UNDERLINED) {
            let _ = queue!(out, SetAttribute(Attribute::Underlined));
        }
        for (m, a) in [
            (Modifier::DIM, Attribute::Dim),
            (Modifier::BOLD, Attribute::Bold),
            (Modifier::ITALIC, Attribute::Italic),
            (Modifier::REVERSED, Attribute::Reverse),
        ] {
            if l.modifier.contains(m) {
                let _ = queue!(out, SetAttribute(a));
            }
        }
        let _ = queue!(
            out,
            Print(format!(
                "\x1b]8;;{}\x1b\\",
                l.url.replace(['\x1b', '\x07'], "")
            )),
            Print(&l.text),
            Print("\x1b]8;;\x1b\\"),
            SetAttribute(Attribute::Reset),
            ResetColor
        );
    }
    let _ = queue!(out, RestorePosition);
    let _ = out.flush();
}

/// Open `url` in the system browser, detached.
pub fn open(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("only http(s) links open".into());
    }
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{cmd}: {e}"))
}
