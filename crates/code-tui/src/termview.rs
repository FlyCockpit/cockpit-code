//! An owned terminal screen for the right sidebar (D144 user terminals,
//! D140 tty jobs). Bytes are parsed by `vt100` into this struct's grid and
//! painted as plain cells: no escape sequence ever reaches the host
//! terminal, and title/clipboard/bell requests from the program are
//! dropped (vt100's default callbacks do nothing). Private screens are
//! dropped with the view; nothing here is Debug, serialized or copied to
//! the clipboard.
use crate::engine::TermInfo;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::num::NonZeroU16;

/// Rows of scrollback kept per view.
const SCROLLBACK: usize = 5000;

/// A line-granular local selection, in absolute rows (0 = the oldest
/// scrollback row). It is only ever used to stage an excerpt for sharing;
/// it never reaches the clipboard.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TermSel {
    pub anchor: usize,
    pub focus: usize,
}

impl TermSel {
    pub fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }
}

pub struct TermView {
    pub info: TermInfo,
    parser: vt100::Parser,
    /// The next byte offset expected from the stream.
    pub next: u64,
    /// Earlier output was dropped (snapshot cap, lost chunks).
    pub lost: bool,
    pub sel: Option<TermSel>,
    /// The size last sent to the PTY.
    pub pty_size: Option<(u16, u16)>,
    /// New output since the user last looked at this tab.
    pub unread: bool,
    /// The user closed it from this UI (else: cancel, shutdown, exit).
    pub closing: bool,
    /// The last share mode and staged size seen before close (for the
    /// transcript row).
    pub last_output: bool,
    pub last_staged: usize,
    /// The user downgraded output → status.
    pub made_private: bool,
    /// When the shell exited by itself (the close hint follows shortly).
    pub exited_at: Option<std::time::Instant>,
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap_or(NonZeroU16::MIN)
}

impl TermView {
    pub fn new(info: TermInfo, rows: u16, cols: u16) -> Self {
        Self {
            last_output: info.share_output,
            last_staged: info.staged,
            info,
            parser: vt100::Parser::new(nz(rows), nz(cols), SCROLLBACK),
            next: 0,
            lost: false,
            sel: None,
            pty_size: None,
            unread: false,
            closing: false,
            made_private: false,
            exited_at: None,
        }
    }

    /// Feed stream bytes at `offset`. Returns false on a gap (the caller
    /// resynchronizes); overlapping bytes already shown are skipped.
    pub fn feed(&mut self, offset: u64, bytes: &[u8]) -> bool {
        if offset > self.next {
            return false;
        }
        let skip = (self.next - offset) as usize;
        if skip >= bytes.len() {
            return true;
        }
        self.process(&bytes[skip..]);
        self.next = offset + bytes.len() as u64;
        true
    }

    fn process(&mut self, bytes: &[u8]) {
        let (len, back) = self.scroll_state();
        self.parser.process(bytes);
        // A scrolled-back view keeps showing the same rows.
        if back > 0 {
            let grown = self.parser.screen().scrollback_len().saturating_sub(len);
            self.parser.screen_mut().set_scrollback(back + grown);
        }
        self.unread = true;
    }

    /// Replace the screen with a fresh one fed from `bytes` (a snapshot
    /// reset after a gap, or a job's replay from offset 0).
    pub fn reset(&mut self, offset: u64, bytes: &[u8], truncated: bool) {
        let (rows, cols) = self.size();
        self.parser = vt100::Parser::new(nz(rows), nz(cols), SCROLLBACK);
        self.sel = None;
        self.lost |= truncated || offset > 0;
        self.parser.process(bytes);
        self.next = offset + bytes.len() as u64;
    }

    /// Drop every private byte this view holds (screen, scrollback,
    /// selection).
    pub fn purge(&mut self) {
        self.parser = vt100::Parser::new(nz(1), nz(1), 0);
        self.sel = None;
    }

    pub fn size(&self) -> (u16, u16) {
        let (r, c) = self.parser.screen().size();
        (r.get(), c.get())
    }

    pub fn set_size(&mut self, rows: u16, cols: u16) {
        if self.size() != (rows, cols) {
            self.parser.set_size(nz(rows), nz(cols));
        }
    }

    pub fn alternate(&self) -> bool {
        self.parser.screen().alternate_screen()
    }
    pub fn app_cursor(&self) -> bool {
        self.parser.screen().application_cursor()
    }
    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    /// (scrollback rows, current offset back from the live screen).
    pub fn scroll_state(&self) -> (usize, usize) {
        let s = self.parser.screen();
        (s.scrollback_len(), s.scrollback())
    }
    pub fn scroll_to(&mut self, back: usize) {
        self.parser.screen_mut().set_scrollback(back);
    }
    pub fn scroll_by(&mut self, delta: isize) {
        let (len, back) = self.scroll_state();
        let back = (back as isize - delta).clamp(0, len as isize) as usize;
        self.scroll_to(back);
    }

    /// The absolute row shown at visible row `r`.
    pub fn abs_row(&self, r: u16) -> usize {
        let (len, back) = self.scroll_state();
        len - back + r as usize
    }
    /// The absolute row of the cursor (live screen).
    pub fn cursor_abs(&self) -> usize {
        let (len, _) = self.scroll_state();
        len + self.parser.screen().cursor_position().0 as usize
    }
    pub fn last_abs(&self) -> usize {
        let (len, _) = self.scroll_state();
        len + self.size().0.saturating_sub(1) as usize
    }

    /// Make absolute row `abs` visible.
    pub fn reveal(&mut self, abs: usize) {
        let (len, back) = self.scroll_state();
        let rows = self.size().0 as usize;
        let top = len - back;
        if abs < top {
            self.scroll_to(len - abs);
        } else if abs >= top + rows {
            self.scroll_to((len + rows).saturating_sub(abs + 1).min(len));
        }
    }

    /// The text of the selected rows (trailing blanks trimmed).
    pub fn selection_text(&mut self) -> Option<String> {
        let (a, b) = self.sel?.range();
        let (len, back) = self.scroll_state();
        let (rows, cols) = self.size();
        let rows = rows as usize;
        let mut lines = Vec::with_capacity(b - a + 1);
        let mut abs = a;
        while abs <= b {
            // Bring `abs` to the top of the view (or as near as possible).
            let off = len.saturating_sub(abs);
            self.scroll_to(off);
            let top = len - off;
            for (r, text) in self.parser.screen().rows(0, cols).enumerate() {
                let at = top + r;
                if at >= abs && at <= b {
                    lines.push(text.trim_end().to_string());
                }
                if r + 1 >= rows {
                    break;
                }
            }
            abs = top + rows;
        }
        self.scroll_to(back);
        let text = lines.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    /// Paint the screen into `area` (cursor shown when `cursor`).
    pub fn render(&self, buf: &mut Buffer, area: Rect, cursor: bool, sel_bg: Color) {
        let screen = self.parser.screen();
        let sel = self.sel.map(|s| s.range());
        let (len, back) = self.scroll_state();
        let top = len - back;
        let (crow, ccol) = screen.cursor_position();
        let show_cursor = cursor && back == 0 && !screen.hide_cursor();
        for y in 0..area.height {
            let selected = sel.is_some_and(|(a, b)| (a..=b).contains(&(top + y as usize)));
            let mut x = 0;
            while x < area.width {
                let Some(out) = buf.cell_mut((area.x + x, area.y + y)) else {
                    break;
                };
                out.reset();
                let Some(cell) = screen.cell(y, x) else {
                    if selected {
                        out.set_bg(sel_bg);
                    }
                    x += 1;
                    continue;
                };
                if cell.is_wide_continuation() {
                    x += 1;
                    continue;
                }
                let text = cell.contents();
                // Only printable text reaches the buffer.
                let sym = if text.is_empty() || text.chars().any(char::is_control) {
                    " "
                } else {
                    text
                };
                out.set_symbol(sym);
                let mut style = Style::default()
                    .fg(color(cell.fgcolor()))
                    .bg(color(cell.bgcolor()));
                let mut m = Modifier::empty();
                if cell.bold() {
                    m |= Modifier::BOLD;
                }
                if cell.dim() {
                    m |= Modifier::DIM;
                }
                if cell.italic() {
                    m |= Modifier::ITALIC;
                }
                if cell.underline() {
                    m |= Modifier::UNDERLINED;
                }
                if cell.inverse() {
                    m |= Modifier::REVERSED;
                }
                if show_cursor && y == crow && x == ccol {
                    m ^= Modifier::REVERSED;
                }
                style = style.add_modifier(m);
                if selected {
                    style = style.bg(sel_bg);
                }
                out.set_style(style);
                x += if cell.is_wide() { 2 } else { 1 };
            }
        }
    }

    /// Where the host cursor goes while typing: (col, row) in the area.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        let screen = self.parser.screen();
        let (_, back) = self.scroll_state();
        if back > 0 || screen.hide_cursor() {
            return None;
        }
        let (r, c) = screen.cursor_position();
        Some((c, r))
    }
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// The bytes a terminal would send for `key` (xterm encoding).
pub fn key_bytes(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let modp = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi = |final_: &str| -> Vec<u8> {
        if modp > 1 {
            format!("\x1b[1;{modp}{final_}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{final_}").into_bytes()
        } else {
            format!("\x1b[{final_}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if modp > 1 {
            format!("\x1b[{n};{modp}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let mut out = match key.code {
        KeyCode::Char(c) if ctrl => {
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                '@' | ' ' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                '8' | '?' => 0x7f,
                _ => return None,
            };
            vec![b]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if ctrl => vec![0x08],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi("A"),
        KeyCode::Down => csi("B"),
        KeyCode::Right => csi("C"),
        KeyCode::Left => csi("D"),
        KeyCode::Home => csi("H"),
        KeyCode::End => csi("F"),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let f = b"PQRS"[n as usize - 1] as char;
            if modp > 1 {
                format!("\x1b[1;{modp}{f}").into_bytes()
            } else {
                format!("\x1bO{f}").into_bytes()
            }
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        _ => return None,
    };
    // Alt prefixes plain characters and control bytes with ESC.
    if alt
        && matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace
        )
    {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// Ctrl+] (also reported as Ctrl+5 by legacy terminals): leaves the
/// terminal; never sent to the PTY.
pub fn is_leave_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
}
