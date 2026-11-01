//! Reusable modal dialog: title, a markdown or diff body, a list of options
//! and an optional free-text field. Adapted from FlyCockpit's Cockpit dialog state
//! machine reduced to one page; approvals, questions and the session-lock
//! dialog will reuse it.
use crate::composer::TextBuf;
use crate::theme::theme;
use crate::ui::{self, Hits, ScrollId, Target};
use crate::wrap::wrap_lines;
use crate::{diff, markdown};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

#[derive(Clone)]
pub struct DialogOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    /// Single-key shortcut that chooses this option directly.
    pub key: Option<char>,
}

impl DialogOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            key: None,
        }
    }
    pub fn key(mut self, key: char) -> Self {
        self.key = Some(key);
        self
    }
    pub fn describe(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }
}

#[allow(dead_code)]
pub enum Body {
    None,
    Markdown(String),
    Diff {
        old: String,
        new: String,
    },
    FileDiff {
        path: String,
        old: String,
        new: String,
        start: Option<(usize, usize)>,
    },
    Parts(Vec<Body>),
}

impl Body {
    fn render(&self, width: usize) -> Vec<Line<'static>> {
        match self {
            Self::None => vec![],
            Self::Markdown(md) => markdown::render(md, width)
                .into_iter()
                .map(|line| line.style(theme().text()))
                .collect(),
            Self::Diff { old, new } => diff::render(old, new).lines,
            Self::FileDiff {
                path,
                old,
                new,
                start,
            } => {
                let d = diff::render_file(path, old, new, *start);
                wrap_lines(d.lines, width, &[], &[Span::raw(" ".repeat(d.indent))])
            }
            Self::Parts(parts) => {
                let mut lines = Vec::new();
                for part in parts {
                    let rendered = part.render(width);
                    if !lines.is_empty() && !rendered.is_empty() {
                        lines.push(Line::default());
                    }
                    lines.extend(rendered);
                }
                lines
            }
        }
    }
}

pub enum Outcome {
    Pending,
    Cancelled,
    Chosen { id: String, text: Option<String> },
}

pub struct Dialog {
    pub title: String,
    pub body: Body,
    pub options: Vec<DialogOption>,
    pub selected: usize,
    /// Free-text field (placeholder, contents); focused with Tab.
    pub text: Option<(String, TextBuf)>,
    text_focus: bool,
    /// Option that Enter in the text field chooses (default: the selected one).
    text_option: Option<String>,
    /// Options that need text: choosing one with an empty field focuses
    /// the field, and Enter there chooses it.
    needs_text: Vec<String>,
    /// The field was prefilled: choosing its option edits it first.
    edit_first: bool,
    /// Body scroll (top row) and the visible body height from the last render.
    body_top: usize,
    body_view: usize,
    body_len: usize,
    /// Keys typed ahead (a dialog that opened while the user was typing)
    /// must not choose options: until a short pause, typing keys are
    /// swallowed and extend the guard.
    guard: Instant,
    /// False until the user moves the selection (↑/↓) or clicks: typing
    /// keys never choose an option before that (a dialog shown at start-up
    /// can meet keys typed without looking).
    armed: bool,
    /// A typing key was ignored while unarmed: the footer says why.
    nudged: bool,
}

/// The pause that ends a typing burst which ran into a new dialog.
const TYPE_AHEAD: Duration = Duration::from_millis(400);

impl Dialog {
    pub fn new(title: impl Into<String>, body: Body, options: Vec<DialogOption>) -> Self {
        Self {
            title: title.into(),
            body,
            options,
            selected: 0,
            text: None,
            text_focus: false,
            text_option: None,
            needs_text: vec![],
            edit_first: false,
            body_top: 0,
            body_view: 0,
            body_len: 0,
            guard: Instant::now() + TYPE_AHEAD,
            armed: true,
            nudged: false,
        }
    }
    /// Typing keys (hotkeys, Enter) work only after an explicit focus step:
    /// ↑/↓ or a click.
    pub fn require_focus(mut self) -> Self {
        self.armed = false;
        self
    }

    pub fn with_text(mut self, placeholder: impl Into<String>) -> Self {
        self.text = Some((placeholder.into(), TextBuf::default()));
        self
    }
    /// Enter in the text field chooses `id`; `focus` starts with the field focused.
    pub fn text_option(mut self, id: impl Into<String>, focus: bool) -> Self {
        self.text_option = Some(id.into());
        self.text_focus = focus;
        self
    }
    /// The text field starts with `text` (an edit of a proposal).
    pub fn prefill(mut self, text: &str) -> Self {
        if let Some((_, buf)) = &mut self.text {
            buf.set(text);
            buf.end();
            self.edit_first = true;
        }
        self
    }
    /// `ids` need text in the field (a pivot message, an override note).
    pub fn needs_text(mut self, ids: &[&str]) -> Self {
        self.needs_text = ids.iter().map(|s| s.to_string()).collect();
        self
    }
    fn wants_text(&self, idx: usize) -> bool {
        self.options.get(idx).is_some_and(|o| {
            Some(&o.id) == self.text_option.as_ref() || self.needs_text.contains(&o.id)
        })
    }
    pub fn select(mut self, id: &str) -> Self {
        if let Some(i) = self.options.iter().position(|o| o.id == id) {
            self.selected = i;
        }
        self
    }
    fn chosen(&mut self, idx: usize) -> Outcome {
        if self.edit_first && !self.text_focus && self.wants_text(idx) {
            self.selected = idx;
            self.text_focus = true;
            return Outcome::Pending;
        }
        // Choosing the text option with an empty field focuses the field first.
        if self
            .text
            .as_ref()
            .is_some_and(|(_, b)| b.text.trim().is_empty())
            && self.wants_text(idx)
            && (!self.text_focus
                || self
                    .needs_text
                    .iter()
                    .any(|n| Some(n) == self.options.get(idx).map(|o| &o.id)))
        {
            self.selected = idx;
            self.text_focus = true;
            return Outcome::Pending;
        }
        let text = self
            .text
            .as_ref()
            .map(|(_, b)| b.text.clone())
            .filter(|t| !t.trim().is_empty());
        match self.options.get(idx) {
            Some(o) => Outcome::Chosen {
                id: o.id.clone(),
                text,
            },
            None => Outcome::Pending,
        }
    }
    /// A click on option `i` chooses it (as Enter).
    pub fn click(&mut self, i: usize) -> Outcome {
        self.text_focus = false;
        self.armed = true;
        self.chosen(i)
    }
    /// The wheel scrolls a long body.
    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.body_top as isize + delta).max(0) as usize);
    }
    pub fn set_offset(&mut self, top: usize) {
        self.body_top = top.min(self.body_len.saturating_sub(self.body_view));
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        // Typed-ahead keys (characters, Enter, Tab, Backspace) right after
        // the dialog opened belong to whatever the user was typing.
        if matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace
        ) && !key.modifiers.contains(KeyModifiers::CONTROL)
            && Instant::now() < self.guard
        {
            self.guard = Instant::now() + TYPE_AHEAD;
            return Outcome::Pending;
        }
        if !self.armed {
            match key.code {
                KeyCode::Up | KeyCode::Down => self.armed = true,
                KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.nudged = true;
                    return Outcome::Pending;
                }
                _ => {}
            }
        }
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            let page = self.body_view.max(1) as isize;
            self.wheel(if key.code == KeyCode::PageUp {
                -page
            } else {
                page
            });
            return Outcome::Pending;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Outcome::Cancelled;
        }
        if key.code == KeyCode::Tab && self.text.is_some() {
            self.text_focus = !self.text_focus;
            return Outcome::Pending;
        }
        if self.text_focus
            && let Some((_, buf)) = &mut self.text
        {
            match key.code {
                KeyCode::Enter => {
                    let idx = if self.needs_text.is_empty() || !self.wants_text(self.selected) {
                        self.text_option
                            .as_ref()
                            .and_then(|id| self.options.iter().position(|o| &o.id == id))
                            .unwrap_or(self.selected)
                    } else {
                        self.selected
                    };
                    return self.chosen(idx);
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.kill_line()
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.delete_word()
                }
                KeyCode::Char(c) => buf.insert(c.encode_utf8(&mut [0; 4])),
                KeyCode::Backspace => buf.backspace(),
                KeyCode::Delete => buf.delete(),
                KeyCode::Left => buf.left(),
                KeyCode::Right => buf.right(),
                KeyCode::Home => buf.home(),
                KeyCode::End => buf.end(),
                KeyCode::Up | KeyCode::Down => self.text_focus = false,
                _ => {}
            }
            return Outcome::Pending;
        }
        let n = self.options.len().max(1);
        // Option shortcuts win over j/k navigation.
        if let KeyCode::Char(c) = key.code
            && let Some(i) = self.options.iter().position(|o| o.key == Some(c))
        {
            return self.chosen(i);
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = (self.selected + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1) % n,
            KeyCode::Enter => return self.chosen(self.selected),
            KeyCode::Char(c) => {
                if let Some(i) = self.options.iter().position(|o| o.key == Some(c)) {
                    return self.chosen(i);
                }
                if let Some(d) = c.to_digit(10)
                    && (1..=self.options.len()).contains(&(d as usize))
                {
                    return self.chosen(d as usize - 1);
                }
            }
            _ => {}
        }
        Outcome::Pending
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let width = screen.width.saturating_sub(4).clamp(20, 90);
        // One column of the padding is the body scrollbar.
        let inner_w = width.saturating_sub(5) as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let body = self.body.render(inner_w);
        let mut tail: Vec<Line<'static>> = vec![];
        let mut option_rows: Vec<(usize, usize, usize)> = vec![];
        for (i, o) in self.options.iter().enumerate() {
            let sel = i == self.selected && !self.text_focus;
            // While typing, the option the text is for keeps its marker.
            let typing_for = self.text_focus && i == self.selected && self.wants_text(i);
            let marker = if sel || typing_for { "› " } else { "  " };
            let style = if sel {
                ui::chip_style(Style::default(), true)
            } else {
                t.text()
            };
            let key = o.key.map(|k| format!("  {k}")).unwrap_or_default();
            let first = tail.len();
            let head = format!("{marker}{}", o.label);
            // The selected option washes the full row width, like hover.
            let pad = if sel {
                inner_w.saturating_sub(head.width() + key.width())
            } else {
                0
            };
            tail.push(Line::from(vec![
                Span::styled(head, style),
                Span::styled(
                    key,
                    if sel {
                        style.patch(t.faint())
                    } else {
                        t.faint()
                    },
                ),
                Span::styled(" ".repeat(pad), style),
            ]));
            if let Some(d) = &o.description {
                tail.extend(wrap_lines(
                    vec![Line::from(Span::styled(d.clone(), t.faint()))],
                    inner_w,
                    &[Span::raw("    ")],
                    &[Span::raw("    ")],
                ));
            }
            option_rows.push((i, first, tail.len()));
        }
        let mut cursor = None;
        if let Some((placeholder, buf)) = &self.text {
            tail.push(Line::default());
            let row = tail.len() as u16;
            let prefix = Span::styled(
                "✎ ",
                Style::default().fg(if self.text_focus { t.accent } else { t.dim }),
            );
            if buf.text.is_empty() {
                // Focused: say what Enter will do.
                let hint = if self.text_focus {
                    let target = self
                        .text_option
                        .as_ref()
                        .and_then(|id| self.options.iter().find(|o| &o.id == id))
                        .or_else(|| self.options.get(self.selected))
                        .map(|o| o.label.to_lowercase())
                        .unwrap_or_default();
                    format!("{placeholder} · enter: {target} · tab: back to the options")
                } else {
                    format!("{placeholder} (tab)")
                };
                tail.push(Line::from(vec![
                    prefix,
                    Span::styled(
                        crate::wrap::truncate(&hint, inner_w.saturating_sub(2)),
                        t.faint(),
                    ),
                ]));
                if self.text_focus {
                    cursor = Some((2, row));
                }
            } else {
                let (rows, (cx, cy)) = buf.layout(inner_w, prefix, Span::raw("  "), t.text());
                if self.text_focus {
                    cursor = Some((cx, row + cy));
                }
                tail.extend(rows);
            }
        }
        if !self.armed {
            tail.push(Line::default());
            let hint = if self.nudged {
                "typed keys are ignored here until you press ↑ or ↓ (or click)"
            } else {
                "↑/↓ to choose first, then enter or a key · or click"
            };
            tail.push(Line::from(Span::styled(
                crate::wrap::truncate(hint, inner_w),
                Style::default().fg(if self.nudged { t.yellow } else { t.dim }),
            )));
        }
        let body = wrap_lines(body, inner_w, &[], &[]);
        let gap = usize::from(!body.is_empty());
        let max_body = (screen.height as usize)
            .saturating_sub(tail.len() + gap + 4)
            .max(3);
        let view = body.len().min(max_body);
        self.body_len = body.len();
        self.body_view = view;
        self.body_top = self.body_top.min(body.len().saturating_sub(view));
        lines.extend(body.into_iter().skip(self.body_top).take(view));
        if gap > 0 {
            lines.push(Line::default());
        }
        let tail_start = lines.len();
        lines.extend(tail);
        let height = (lines.len() as u16 + 2).min(screen.height);
        let area = Rect {
            x: screen.x + (screen.width.saturating_sub(width)) / 2,
            y: screen.y + (screen.height.saturating_sub(height)) / 2,
            width: width.min(screen.width),
            height,
        };
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
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };
        for (i, a, b) in option_rows {
            let y = inner.y + (tail_start + a) as u16;
            if y >= inner.bottom() {
                break;
            }
            hits.add(
                Rect {
                    y,
                    height: ((b - a) as u16).min(inner.bottom() - y),
                    ..inner
                },
                Target::Row(i),
            );
        }
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::Overlay,
            Rect {
                x: area.right().saturating_sub(2),
                width: 1,
                height: view as u16,
                ..inner
            },
            self.body_len,
            view,
            self.body_top,
        );
        if let Some((x, y)) = cursor {
            frame.set_cursor_position((area.x + 2 + x, area.y + 1 + (tail_start as u16) + y));
        }
    }
}
