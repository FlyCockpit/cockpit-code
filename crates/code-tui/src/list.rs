//! Generic single-select list overlay (session picker, fork-at-message picker).
//! Rows have a main label and a dim detail column; an optional extra key
//! (e.g. `a` to toggle "all sessions") is reported back to the caller.
use crate::theme::theme;
use crate::ui::{self, Hits, ScrollId, Target};
use crate::wrap::truncate;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

pub struct Row {
    pub id: String,
    pub label: String,
    pub detail: String,
    /// Something is wrong (an MCP server error): the detail is red, and
    /// in full under the list while the row is selected.
    pub alert: bool,
}

pub enum Action {
    None,
    Close,
    Choose(String),
    Key(char),
}

pub struct ListPicker {
    pub title: String,
    pub footer: String,
    pub rows: Vec<Row>,
    pub empty: String,
    /// Keys reported as `Action::Key`.
    pub keys: Vec<char>,
    /// Typing filters the rows (the session picker): plain keys go to the
    /// filter, so the picker's own keys use Tab and Ctrl (reported as
    /// `Action::Key`: Tab `'\t'`, Ctrl+R `'r'`, Ctrl+D / Delete `'d'`).
    pub filterable: bool,
    pub filter: String,
    /// Every row; `rows` is the filtered view.
    all: Vec<Row>,
    /// Lines shown under the list for the selected row (a preview).
    pub preview: Vec<Line<'static>>,
    selected: usize,
    scroll: usize,
    view: usize,
    /// The scrollbar set `scroll`; keep it until the next key.
    pinned: bool,
}

impl ListPicker {
    pub fn new(title: impl Into<String>, rows: Vec<Row>) -> Self {
        Self {
            title: title.into(),
            footer: "↑↓ select · enter choose · esc close".into(),
            rows,
            empty: "nothing to show".into(),
            keys: vec![],
            filterable: false,
            filter: String::new(),
            all: vec![],
            preview: vec![],
            selected: 0,
            scroll: 0,
            view: 1,
            pinned: false,
        }
    }
    /// Make typing filter the rows, starting from `filter`.
    pub fn with_filter(mut self, filter: &str) -> Self {
        self.filterable = true;
        self.all = std::mem::take(&mut self.rows);
        self.filter = filter.to_string();
        self.refilter();
        self
    }
    fn refilter(&mut self) {
        let words: Vec<String> = self
            .filter
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let keep = |r: &Row| {
            let hay = format!("{} {}", r.label, r.detail).to_lowercase();
            words.iter().all(|w| hay.contains(w))
        };
        let sel = self.selected_id();
        self.rows = self
            .all
            .iter()
            .filter(|r| keep(r))
            .map(|r| Row {
                id: r.id.clone(),
                label: r.label.clone(),
                detail: r.detail.clone(),
                alert: r.alert,
            })
            .collect();
        self.selected = sel
            .and_then(|id| self.rows.iter().position(|r| r.id == id))
            .unwrap_or(0);
        self.scroll = 0;
    }
    pub fn selected_id(&self) -> Option<String> {
        self.rows.get(self.selected).map(|r| r.id.clone())
    }
    /// Select the row with `id`, if present (after a refresh).
    pub fn select_id(mut self, id: Option<&str>) -> Self {
        if let Some(i) = id.and_then(|id| self.rows.iter().position(|r| r.id == id)) {
            self.selected = i;
        }
        self
    }
    pub fn select_last(mut self) -> Self {
        self.selected = self.rows.len().saturating_sub(1);
        self
    }
    /// A click on row `i`: select it and choose it (as Enter).
    pub fn click(&mut self, i: usize) -> Action {
        match self.rows.get(i) {
            Some(r) => {
                self.selected = i;
                Action::Choose(r.id.clone())
            }
            None => Action::None,
        }
    }
    /// The wheel scrolls the rows; the selection stays (D130).
    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.scroll as isize + delta).max(0) as usize);
    }
    /// Scrollbar or wheel: set the top row. The selection stays where it
    /// is (it may scroll out of view); the next key brings it back.
    pub fn set_offset(&mut self, top: usize) {
        self.scroll = top.min(self.rows.len().saturating_sub(self.view.max(1)));
        self.pinned = true;
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        self.pinned = false;
        let n = self.rows.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.filterable {
            match key.code {
                KeyCode::Esc if !self.filter.is_empty() => {
                    self.filter.clear();
                    self.refilter();
                    return Action::None;
                }
                KeyCode::Tab => return Action::Key('\t'),
                KeyCode::Char('r') if ctrl => return Action::Key('r'),
                KeyCode::Char('d') if ctrl => return Action::Key('d'),
                KeyCode::Delete => return Action::Key('d'),
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refilter();
                    return Action::None;
                }
                KeyCode::Char('u') if ctrl => {
                    self.filter.clear();
                    self.refilter();
                    return Action::None;
                }
                KeyCode::Char(c) if !ctrl => {
                    self.filter.push(c);
                    self.refilter();
                    return Action::None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Action::Close;
            }
            KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                self.selected = self.selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                self.selected = (self.selected + 1).min(n - 1)
            }
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::PageDown if n > 0 => self.selected = (self.selected + 10).min(n - 1),
            KeyCode::Enter => {
                if let Some(r) = self.rows.get(self.selected) {
                    return Action::Choose(r.id.clone());
                }
            }
            KeyCode::Char(c) if self.keys.contains(&c) => return Action::Key(c),
            _ => {}
        }
        Action::None
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let width = width_for(screen);
        // The selected row's problem, in full under the list.
        let alert = self
            .rows
            .get(self.selected)
            .filter(|r| r.alert)
            .map(|r| r.detail.clone());
        let alert_lines = alert.as_ref().map_or(vec![], |a| {
            crate::wrap::wrap_lines(
                vec![Line::from(Span::styled(
                    format!("⚠ {a}"),
                    Style::default().fg(t.red),
                ))],
                (width_for(screen) as usize).saturating_sub(5),
                &[],
                &[Span::raw("  ")],
            )
        });
        let preview_w = (width_for(screen) as usize).saturating_sub(5);
        let preview_lines: Vec<Line<'static>> = if self.preview.is_empty() {
            vec![]
        } else {
            let mut v = crate::wrap::wrap_lines(self.preview.clone(), preview_w, &[], &[]);
            v.truncate(8);
            v
        };
        let filter_line = self.filterable.then(|| {
            Line::from(vec![
                Span::styled("filter › ", Style::default().fg(t.accent)),
                if self.filter.is_empty() {
                    Span::styled("type to filter", t.faint())
                } else {
                    Span::styled(self.filter.clone(), t.text())
                },
                Span::styled(
                    format!("   {}/{}", self.rows.len(), self.all.len()),
                    t.faint(),
                ),
            ])
        });
        let extra = alert_lines.len() as u16
            + u16::from(!alert_lines.is_empty())
            + preview_lines.len() as u16
            + u16::from(!preview_lines.is_empty())
            + 2 * u16::from(filter_line.is_some());
        let height = screen
            .height
            .saturating_sub(4)
            .min(self.rows.len().max(1) as u16 + 4 + extra)
            .max(6);
        let area = Rect {
            x: screen.x + (screen.width.saturating_sub(width)) / 2,
            y: screen.y + (screen.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        // One column of the padding is the scrollbar track.
        let inner_w = width.saturating_sub(5) as usize;
        let view = height.saturating_sub(4 + extra) as usize;
        self.view = view;
        if self.pinned {
        } else if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + view {
            self.scroll = self.selected + 1 - view;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(view));
        let mut lines: Vec<Line<'static>> = vec![];
        if let Some(f) = filter_line {
            lines.push(f);
            lines.push(Line::default());
        }
        let list_top = lines.len() as u16;
        if self.rows.is_empty() {
            let empty = if self.filterable && !self.filter.is_empty() && !self.all.is_empty() {
                "nothing matches the filter · esc clears it".to_string()
            } else {
                self.empty.clone()
            };
            lines.push(Line::from(Span::styled(empty, t.faint())));
        }
        for (i, r) in self.rows.iter().enumerate().skip(self.scroll).take(view) {
            let sel = i == self.selected;
            let style = if sel {
                ui::chip_style(Style::default(), true)
            } else {
                t.text()
            };
            let detail_w = r.detail.chars().count().min(inner_w / 2);
            let label_w = inner_w.saturating_sub(detail_w + 3);
            let label = truncate(&r.label.replace('\n', " "), label_w);
            let pad = label_w.saturating_sub(label.chars().count());
            lines.push(Line::from(vec![
                Span::styled(format!("{} {label}", if sel { "›" } else { " " }), style),
                Span::styled(" ".repeat(pad + 1), style),
                Span::styled(
                    truncate(&r.detail, detail_w),
                    if r.alert {
                        style.remove_modifier(Modifier::BOLD).fg(t.red)
                    } else if sel {
                        style.remove_modifier(Modifier::BOLD).fg(t.fog)
                    } else {
                        t.faint()
                    },
                ),
            ]));
        }
        if !alert_lines.is_empty() {
            lines.push(Line::default());
            lines.extend(alert_lines);
        }
        if !preview_lines.is_empty() {
            // Pad the list so the preview sits under a full-height list.
            let shown = self.rows.len().saturating_sub(self.scroll).min(view).max(1);
            for _ in shown..view {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                "─".repeat(preview_w),
                Style::default().fg(t.rule),
            )));
            lines.extend(preview_lines);
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(self.footer.clone(), t.faint())));
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .title(Span::styled(format!(" {} ", self.title), t.bold()))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(t.surface));
        frame.render_widget(Paragraph::new(lines).block(block), area);
        let list = Rect {
            x: area.x + 2,
            y: area.y + 1 + list_top,
            width: area.width.saturating_sub(4),
            height: view as u16,
        };
        for (n, i) in (self.scroll..self.rows.len()).take(view).enumerate() {
            hits.add(
                Rect {
                    y: list.y + n as u16,
                    height: 1,
                    ..list
                },
                Target::Row(i),
            );
        }
        let bar = Rect {
            x: area.right().saturating_sub(2),
            width: 1,
            ..list
        };
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::Overlay,
            bar,
            self.rows.len(),
            view,
            self.scroll,
        );
    }
}

fn width_for(screen: Rect) -> u16 {
    screen.width.saturating_sub(4).min(110)
}
