//! Paged read-only text overlay (job output).
use crate::theme::theme;
use crate::ui::{self, Hits, ScrollId};
use crate::wrap::truncate;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

pub enum Action {
    None,
    Close,
    Refresh,
    Kill,
    /// Open the text in `$EDITOR` (a read-only copy).
    Edit,
}

pub struct Pager {
    pub title: String,
    pub id: String,
    /// Job output (refresh, kill, back to /jobs); else a read-only text
    /// (a paste) that opens at the top.
    pub job: bool,
    lines: Vec<String>,
    /// The text as given (for `$EDITOR`).
    pub full: String,
    /// Top line; None follows the end.
    top: Option<usize>,
    view: usize,
    /// A line to highlight (a `file:line` location), 0-based.
    mark: Option<usize>,
}

impl Pager {
    pub fn new(title: impl Into<String>, id: impl Into<String>, text: &str) -> Self {
        let mut p = Self {
            title: title.into(),
            id: id.into(),
            job: true,
            lines: vec![],
            full: String::new(),
            top: None,
            view: 10,
            mark: None,
        };
        p.set_text(text);
        p
    }
    /// A read-only text box (a paste), from the top.
    pub fn text(title: impl Into<String>, text: &str) -> Self {
        let mut p = Self::new(title, "", text);
        p.job = false;
        p.top = Some(0);
        p.full = text.to_string();
        p
    }
    /// A file, read-only, with line numbers; `line` (1-based) is
    /// highlighted and scrolled to (a finding location, a saved output).
    pub fn file(path: &std::path::Path, line: Option<usize>) -> Result<Self, String> {
        use std::io::Read;
        const CAP: u64 = 8 << 20;
        let mut bytes = vec![];
        std::fs::File::open(path)
            .and_then(|f| f.take(CAP).read_to_end(&mut bytes))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        let n = text.lines().count().max(1);
        let w = n.to_string().len();
        let numbered: String = text
            .lines()
            .enumerate()
            .map(|(i, l)| format!("{:>w$} │ {l}\n", i + 1))
            .collect();
        // Outside the cwd (a saved output): the last two components.
        let mut name = crate::transcript::rel_path(path);
        if name.starts_with('/') {
            let parts: Vec<_> = path.iter().map(|p| p.to_string_lossy()).collect();
            name = format!("…/{}", parts[parts.len().saturating_sub(2)..].join("/"));
        }
        let title = format!(
            "{name} · read-only{}",
            if bytes.len() as u64 >= CAP {
                " · first 8 MB"
            } else {
                ""
            }
        );
        let mut p = Self::text(title, &numbered);
        p.full = text.into_owned();
        if let Some(l) = line.filter(|l| *l > 0) {
            p.mark = Some(l - 1);
            p.top = Some(l.saturating_sub(6));
        }
        Ok(p)
    }
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.lines().map(|l| l.replace('\t', "    ")).collect();
        if self.lines.is_empty() {
            self.lines.push("(no output yet)".into());
        }
    }
    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.view)
    }
    fn scroll(&mut self, delta: isize) {
        let top = (self.top.unwrap_or(self.max_top()) as isize + delta)
            .clamp(0, self.max_top() as isize) as usize;
        self.top = (top < self.max_top()).then_some(top);
    }
    pub fn wheel(&mut self, delta: isize) {
        self.scroll(delta);
    }
    pub fn set_offset(&mut self, top: usize) {
        let top = top.min(self.max_top());
        self.top = (top < self.max_top()).then_some(top);
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        let page = self.view.saturating_sub(1).max(1) as isize;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Action::Close;
            }
            KeyCode::Char('r') if self.job => return Action::Refresh,
            KeyCode::Char('x') if self.job => return Action::Kill,
            KeyCode::Char('e') if !self.job => return Action::Edit,
            KeyCode::Up => self.scroll(-1),
            KeyCode::Down => self.scroll(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll(page),
            KeyCode::Home | KeyCode::Char('g') => self.top = Some(0),
            KeyCode::End | KeyCode::Char('G') => self.top = None,
            _ => {}
        }
        Action::None
    }
    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let area = Rect {
            x: screen.x + 2,
            y: screen.y + 1,
            width: screen.width.saturating_sub(4),
            height: screen.height.saturating_sub(2),
        };
        let inner_w = area.width.saturating_sub(5) as usize;
        self.view = area.height.saturating_sub(4) as usize;
        let top = self.top.unwrap_or(self.max_top()).min(self.max_top());
        let mut lines: Vec<Line<'static>> = self
            .lines
            .iter()
            .enumerate()
            .skip(top)
            .take(self.view)
            .map(|(i, l)| {
                if self.mark == Some(i) {
                    // The location's line: a pointer in the gutter, washed.
                    let l = l.replacen(" │ ", " › ", 1);
                    return Line::from(Span::styled(
                        truncate(&l, inner_w),
                        ui::chip_style(Style::default(), true),
                    ));
                }
                Line::from(Span::styled(truncate(l, inner_w), t.text()))
            })
            .collect();
        let keys = if self.job {
            "r refresh · x kill · esc back"
        } else {
            "e open in $EDITOR · esc close"
        };
        while lines.len() < self.view {
            lines.push(Line::default());
        }
        let end = (top + self.view).min(self.lines.len());
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            truncate(
                &format!(
                    "lines {}-{end} of {} · ↑↓ pgup/pgdn g/G scroll · {keys}",
                    top + 1,
                    self.lines.len()
                ),
                inner_w,
            ),
            t.faint(),
        )));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .title(Span::styled(format!(" {} ", self.title), t.bold()))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(t.surface));
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        frame.render_widget(Paragraph::new(lines).block(block), area);
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::Overlay,
            Rect {
                x: area.right().saturating_sub(2),
                y: area.y + 1,
                width: 1,
                height: self.view as u16,
            },
            self.lines.len(),
            self.view,
            top,
        );
    }
}
