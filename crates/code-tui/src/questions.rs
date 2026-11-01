//! Structured questions (D62): one dialog per `question` call with a tab
//! per question and a single Submit. Choice questions show radio options
//! (checkboxes with `multiSelect`) plus a free-text "Other…" field; boolean
//! questions show Yes / No plus an optional note; text questions show a
//! field. Any question can be skipped. Every option is clickable (with the
//! shared hover) and keyboard-driven: number keys pick options.
use crate::composer::TextBuf;
use crate::engine::{QAnswer, QItem, QKind};
use crate::markdown;
use crate::theme::theme;
use crate::ui::{self, Hits, ScrollId, Target};
use crate::wrap::{truncate, wrap_lines};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

/// Action chips under every page.
pub const ACT_SUBMIT: u8 = 0;
pub const ACT_LATER: u8 = 1;
pub const ACT_JUMP: u8 = 2;
pub const ACT_DISCUSS: u8 = 3;
pub const ACT_FORK: u8 = 4;

pub enum Outcome {
    Pending,
    /// Put off: `/answer` reopens it.
    Later,
    /// Open this asker's transcript; the dialog stays queued.
    Jump(String),
    /// "Chat about this" (D64) for this call.
    Discuss(String),
    /// "Chat about this in a fork" (D64) for this call.
    ForkChat(String),
    /// Answers per call; calls left untouched stay pending (D64).
    Submit(Vec<(String, Vec<QAnswer>)>),
}

/// One `question` call in the dialog.
pub struct Call {
    pub id: String,
    pub asker: String,
    /// The asker's display label.
    pub label: String,
    /// "asked by …" and escalation note (markdown), shown above its pages.
    pub preamble: String,
    /// Not the open view: "Jump to agent" is offered.
    pub can_jump: bool,
}

/// Answer state of one question.
#[derive(Default)]
struct State {
    picked: Vec<bool>,
    /// Choice: the "Other…" text; boolean: the note; text: the answer.
    field: TextBuf,
    yes: Option<bool>,
    skipped: bool,
}

/// One row of the current page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    Option(usize),
    Yes,
    No,
    Field,
    Skip,
}

pub struct QDialog {
    pub title: String,
    pub calls: Vec<Call>,
    /// The call of each item.
    item_call: Vec<usize>,
    pub items: Vec<QItem>,
    state: Vec<State>,
    tab: usize,
    /// Focused row of the page; `rows().len()` and `+1` are the Submit
    /// and later chips.
    focus: usize,
    body_top: usize,
    body_view: usize,
    body_len: usize,
}

impl QDialog {
    /// One dialog over several calls (the pending-questions dialog, D64)
    /// or a single call (D62).
    pub fn new(title: String, calls: Vec<(Call, Vec<QItem>)>) -> Self {
        let mut items = vec![];
        let mut item_call = vec![];
        let mut cs = vec![];
        for (k, (call, qs)) in calls.into_iter().enumerate() {
            item_call.extend(std::iter::repeat_n(k, qs.len()));
            items.extend(qs);
            cs.push(call);
        }
        let state = items
            .iter()
            .map(|q| State {
                picked: vec![false; q.options.len()],
                ..State::default()
            })
            .collect();
        Self {
            title,
            calls: cs,
            item_call,
            items,
            state,
            tab: 0,
            focus: 0,
            body_top: 0,
            body_view: 0,
            body_len: 0,
        }
    }

    pub fn has_call(&self, id: &str) -> bool {
        self.calls.iter().any(|c| c.id == id)
    }

    fn call(&self) -> &Call {
        &self.calls[self.item_call[self.tab]]
    }

    /// The action chips for the current question's call.
    fn acts(&self) -> Vec<(String, u8)> {
        let unanswered = (0..self.items.len()).filter(|&i| !self.answered(i)).count();
        let submit = if self.calls.len() > 1 {
            let touched = (0..self.calls.len())
                .filter(|&c| {
                    (0..self.items.len()).any(|i| self.item_call[i] == c && self.answered(i))
                })
                .count();
            format!(" Submit ({touched} of {} answered) ", self.calls.len())
        } else if unanswered > 0 && self.items.len() > 1 {
            format!(" Submit · {unanswered} unanswered ")
        } else {
            " Submit ".into()
        };
        let mut acts = vec![
            (submit, ACT_SUBMIT),
            (" Answer later ".into(), ACT_LATER),
            (" Chat about this ".into(), ACT_DISCUSS),
            (" Chat in a fork ".into(), ACT_FORK),
        ];
        if self.call().can_jump {
            acts.push((" Jump to agent ".into(), ACT_JUMP));
        }
        acts
    }

    fn rows(&self) -> Vec<Row> {
        let q = &self.items[self.tab];
        let mut rows = match q.kind {
            QKind::Choice => {
                let mut r: Vec<Row> = (0..q.options.len()).map(Row::Option).collect();
                r.push(Row::Field);
                r
            }
            QKind::Boolean => vec![Row::Yes, Row::No, Row::Field],
            QKind::Text => vec![Row::Field],
        };
        rows.push(Row::Skip);
        rows
    }

    fn in_field(&self) -> bool {
        self.rows().get(self.focus) == Some(&Row::Field)
    }

    /// Whether question `i` has an answer (or is skipped).
    fn answered(&self, i: usize) -> bool {
        let s = &self.state[i];
        s.skipped
            || match self.items[i].kind {
                QKind::Choice => s.picked.iter().any(|&p| p) || !s.field.text.trim().is_empty(),
                QKind::Boolean => s.yes.is_some(),
                QKind::Text => !s.field.text.trim().is_empty(),
            }
    }

    /// Answers per call. With several calls only those with at least one
    /// answered (or skipped) question are sent; the rest stay pending.
    pub fn answers(&self) -> Vec<(String, Vec<QAnswer>)> {
        let all = self.item_answers();
        (0..self.calls.len())
            .filter(|&c| {
                self.calls.len() == 1
                    || (0..self.items.len()).any(|i| self.item_call[i] == c && self.answered(i))
            })
            .map(|c| {
                let answers = (0..self.items.len())
                    .filter(|&i| self.item_call[i] == c)
                    .map(|i| all[i].clone())
                    .collect();
                (self.calls[c].id.clone(), answers)
            })
            .collect()
    }

    fn item_answers(&self) -> Vec<QAnswer> {
        (0..self.items.len())
            .map(|i| {
                let (q, s) = (&self.items[i], &self.state[i]);
                if s.skipped || !self.answered(i) {
                    return QAnswer::Skipped;
                }
                let field = s.field.text.trim().to_string();
                match q.kind {
                    QKind::Choice => QAnswer::Choice {
                        labels: q
                            .options
                            .iter()
                            .zip(&s.picked)
                            .filter(|(_, p)| **p)
                            .map(|(o, _)| o.label.clone())
                            .collect(),
                        other: (!field.is_empty()).then_some(field),
                    },
                    QKind::Boolean => QAnswer::Boolean {
                        value: s.yes.unwrap_or(false),
                        note: (!field.is_empty()).then_some(field),
                    },
                    QKind::Text => QAnswer::Text(field),
                }
            })
            .collect()
    }

    fn go_tab(&mut self, tab: usize) {
        self.tab = tab.min(self.items.len() - 1);
        self.focus = 0;
        self.body_top = 0;
    }

    /// After answering the current question: the next unanswered one, else
    /// the Submit chip.
    fn advance(&mut self) {
        if let Some(next) = (self.tab + 1..self.items.len()).find(|&i| !self.answered(i)) {
            self.go_tab(next);
        } else {
            self.focus = self.rows().len();
        }
    }

    /// Choose option row `row` (click, number key, Enter or space).
    fn pick(&mut self, row: Row, advance: bool) {
        let tab = self.tab;
        let multi = self.items[tab].multi;
        let s = &mut self.state[tab];
        s.skipped = false;
        match row {
            Row::Option(i) => {
                if multi {
                    s.picked[i] = !s.picked[i];
                    return;
                }
                s.picked.iter_mut().for_each(|p| *p = false);
                s.picked[i] = true;
                // A single choice replaces a typed "Other".
                s.field.clear();
            }
            Row::Yes => s.yes = Some(true),
            Row::No => s.yes = Some(false),
            Row::Skip => {
                s.skipped = !s.skipped;
                if s.skipped {
                    self.advance();
                }
                return;
            }
            Row::Field => return,
        }
        if advance {
            self.advance();
        }
    }

    pub fn click_row(&mut self, k: usize) -> Outcome {
        let rows = self.rows();
        let Some(&row) = rows.get(k) else {
            return Outcome::Pending;
        };
        self.focus = k;
        if row != Row::Field {
            self.pick(row, false);
        }
        Outcome::Pending
    }

    pub fn click_tab(&mut self, i: usize) {
        if i < self.items.len() {
            self.go_tab(i);
        }
    }

    pub fn act(&mut self, a: u8) -> Outcome {
        let call = self.call();
        match a {
            ACT_SUBMIT => Outcome::Submit(self.answers()),
            ACT_JUMP => Outcome::Jump(call.asker.clone()),
            ACT_DISCUSS => Outcome::Discuss(call.id.clone()),
            ACT_FORK => Outcome::ForkChat(call.id.clone()),
            _ => Outcome::Later,
        }
    }

    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.body_top as isize + delta).max(0) as usize);
    }
    pub fn set_offset(&mut self, top: usize) {
        self.body_top = top.min(self.body_len.saturating_sub(self.body_view));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let rows = self.rows();
        let n = rows.len();
        let acts = self.acts();
        let chips = acts.len();
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
            return Outcome::Later;
        }
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            return Outcome::Submit(self.answers());
        }
        match key.code {
            KeyCode::Tab => {
                if self.tab + 1 < self.items.len() {
                    self.go_tab(self.tab + 1);
                } else {
                    self.focus = n;
                }
                return Outcome::Pending;
            }
            KeyCode::BackTab => {
                if self.tab > 0 {
                    self.go_tab(self.tab - 1);
                }
                return Outcome::Pending;
            }
            KeyCode::Up => {
                self.focus = self.focus.saturating_sub(1);
                return Outcome::Pending;
            }
            KeyCode::Down => {
                self.focus = (self.focus + 1).min(n + chips - 1);
                return Outcome::Pending;
            }
            KeyCode::PageUp => {
                self.wheel(-(self.body_view.max(1) as isize));
                return Outcome::Pending;
            }
            KeyCode::PageDown => {
                self.wheel(self.body_view.max(1) as isize);
                return Outcome::Pending;
            }
            _ => {}
        }
        // On the action chips.
        if self.focus >= n {
            let act = self.focus - n;
            match key.code {
                KeyCode::Left => self.focus = (self.focus - 1).max(n),
                KeyCode::Right => self.focus = (self.focus + 1).min(n + chips - 1),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    return self.act(acts[act.min(chips - 1)].1);
                }
                _ => {}
            }
            return Outcome::Pending;
        }
        if self.in_field() {
            let buf = &mut self.state[self.tab].field;
            match key.code {
                // Enter on an empty field keeps it focused for typing (it
                // used to advance, so typed letters became hotkeys); with
                // text it answers and moves on.
                KeyCode::Enter if buf.text.trim().is_empty() => {}
                KeyCode::Enter => self.advance(),
                KeyCode::Char(c) if !ctrl => {
                    buf.insert(c.encode_utf8(&mut [0; 4]));
                    let tab = self.tab;
                    let s = &mut self.state[tab];
                    s.skipped = false;
                    // Typing "Other" replaces a single choice.
                    if self.items[tab].kind == QKind::Choice && !self.items[tab].multi {
                        s.picked.iter_mut().for_each(|p| *p = false);
                    }
                }
                KeyCode::Backspace => buf.backspace(),
                KeyCode::Delete => buf.delete(),
                KeyCode::Left => buf.left(),
                KeyCode::Right => buf.right(),
                KeyCode::Home => buf.home(),
                KeyCode::End => buf.end(),
                _ => {}
            }
            return Outcome::Pending;
        }
        match key.code {
            KeyCode::Left => {
                if self.tab > 0 {
                    self.go_tab(self.tab - 1);
                }
            }
            KeyCode::Right => {
                if self.tab + 1 < self.items.len() {
                    self.go_tab(self.tab + 1);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let row = rows[self.focus];
                self.pick(row, key.code == KeyCode::Enter);
            }
            KeyCode::Char('s') => self.pick(Row::Skip, false),
            KeyCode::Char('y') if self.items[self.tab].kind == QKind::Boolean => {
                self.pick(Row::Yes, true)
            }
            KeyCode::Char('n') if self.items[self.tab].kind == QKind::Boolean => {
                self.pick(Row::No, true)
            }
            KeyCode::Char('o') if self.items[self.tab].kind == QKind::Choice => {
                self.focus = rows.iter().position(|r| *r == Row::Field).unwrap_or(0);
            }
            KeyCode::Char('j') if self.call().can_jump => {
                return Outcome::Jump(self.call().asker.clone());
            }
            KeyCode::Char(c) => {
                if let Some(d) = c.to_digit(10).filter(|d| *d >= 1) {
                    let k = d as usize - 1;
                    let choices: Vec<Row> = rows
                        .iter()
                        .copied()
                        .filter(|r| matches!(r, Row::Option(_) | Row::Yes | Row::No))
                        .collect();
                    if let Some(&row) = choices.get(k) {
                        self.focus = rows.iter().position(|r| *r == row).unwrap_or(0);
                        let multi = self.items[self.tab].multi;
                        self.pick(row, !multi);
                    }
                }
            }
            _ => {}
        }
        Outcome::Pending
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let width = screen.width.saturating_sub(4).clamp(24, 92);
        let inner_w = width.saturating_sub(5) as usize;
        let rows = self.rows();
        let q = &self.items[self.tab];
        let s = &self.state[self.tab];
        // Tabs (one per question) when there are several.
        let mut head: Vec<Line<'static>> = vec![];
        let mut tab_cells: Vec<(u16, u16, usize)> = vec![];
        if self.items.len() > 1 {
            let mut spans = vec![];
            let mut x = 0u16;
            // Every tab fits: short names keep their length and the long
            // ones share what is left (trimming the longest first).
            let who = if self.calls.len() > 1 { 8 } else { 0 };
            let names: Vec<String> = self
                .items
                .iter()
                .map(|item| {
                    item.text
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .filter(|c| !matches!(c, '*' | '_' | '`'))
                        .collect()
                })
                .collect();
            let avail = inner_w.saturating_sub(self.items.len() * (8 + who));
            let mut budgets: Vec<usize> = names.iter().map(|n| n.width().max(3)).collect();
            while budgets.iter().sum::<usize>() > avail {
                let Some((k, &m)) = budgets.iter().enumerate().max_by_key(|(_, b)| **b) else {
                    break;
                };
                if m <= 3 {
                    break;
                }
                budgets[k] -= 1;
            }
            for i in 0..self.items.len() {
                let mark = if self.state[i].skipped {
                    " ⊘"
                } else if self.answered(i) {
                    " ✓"
                } else {
                    ""
                };
                let name = truncate(&names[i], budgets[i]);
                let label = if self.calls.len() > 1 {
                    let who = &self.calls[self.item_call[i]].label;
                    format!(" {} {who}: {name}{mark} ", i + 1)
                } else {
                    format!(" {} {name}{mark} ", i + 1)
                };
                let style = if i == self.tab {
                    ui::chip_style(Style::default(), true)
                } else if self.answered(i) {
                    Style::default().fg(t.green)
                } else {
                    t.muted()
                };
                let w = label.width() as u16;
                if (x + w) as usize > inner_w {
                    break;
                }
                tab_cells.push((x, w, i));
                spans.push(Span::styled(label, style));
                spans.push(Span::raw(" "));
                x += w + 1;
            }
            head.push(Line::from(spans));
            head.push(Line::default());
        }
        // Body: preamble and the question text (scrolls when long).
        let mut body_md = String::new();
        let call = self.call();
        if self.calls.len() > 1 && call.preamble.is_empty() {
            body_md.push_str(&format!("from **{}**\n\n", call.label));
        }
        if !call.preamble.is_empty() {
            body_md.push_str(&call.preamble);
            body_md.push_str("\n\n");
        }
        body_md.push_str(&q.text);
        if q.multi {
            body_md.push_str("\n\n_choose any number_");
        }
        let body: Vec<Line<'static>> = wrap_lines(
            markdown::render(&body_md, inner_w)
                .into_iter()
                .map(|l| l.style(t.text()))
                .collect(),
            inner_w,
            &[],
            &[],
        );
        // Option rows.
        let mut tail: Vec<Line<'static>> = vec![Line::default()];
        let mut row_spans: Vec<(usize, usize, usize)> = vec![];
        let mut cursor = None;
        for (k, row) in rows.iter().enumerate() {
            let focused = k == self.focus;
            let marker = if focused { "› " } else { "  " };
            let base = if focused {
                ui::chip_style(Style::default(), true)
            } else {
                t.text()
            };
            let first = tail.len();
            match *row {
                Row::Option(i) => {
                    let on = s.picked[i];
                    let glyph = match (q.multi, on) {
                        (true, true) => "☑",
                        (true, false) => "☐",
                        (false, true) => "◉",
                        (false, false) => "○",
                    };
                    tail.push(Line::from(vec![
                        Span::styled(format!("{marker}{glyph} {}", q.options[i].label), base),
                        Span::styled(format!("  {}", i + 1), t.faint()),
                    ]));
                    if let Some(d) = &q.options[i].description {
                        tail.extend(wrap_lines(
                            vec![Line::from(Span::styled(d.clone(), t.faint()))],
                            inner_w,
                            &[Span::raw("      ")],
                            &[Span::raw("      ")],
                        ));
                    }
                }
                Row::Yes | Row::No => {
                    let yes = *row == Row::Yes;
                    let on = s.yes == Some(yes);
                    tail.push(Line::from(vec![
                        Span::styled(
                            format!(
                                "{marker}{} {}",
                                if on { "◉" } else { "○" },
                                if yes { "Yes" } else { "No" }
                            ),
                            base,
                        ),
                        Span::styled(
                            format!(
                                "  {} · {}",
                                if yes { 1 } else { 2 },
                                if yes { "y" } else { "n" }
                            ),
                            t.faint(),
                        ),
                    ]));
                }
                Row::Field => {
                    let label = match q.kind {
                        QKind::Choice => "Other",
                        QKind::Boolean => "Note (optional)",
                        QKind::Text => "Answer",
                    };
                    let prefix = Span::styled(
                        format!("{marker}✎ {label}: "),
                        if focused {
                            base
                        } else {
                            Style::default().fg(t.fog)
                        },
                    );
                    let pw = prefix.content.width();
                    if s.field.text.is_empty() {
                        tail.push(Line::from(vec![
                            prefix,
                            Span::styled(
                                if focused { "type here" } else { "" }.to_string(),
                                t.faint(),
                            ),
                        ]));
                        if focused {
                            cursor = Some((pw as u16, (tail.len() - 1) as u16));
                        }
                    } else {
                        let (lines, (cx, cy)) =
                            s.field
                                .layout(inner_w, prefix, Span::raw(" ".repeat(pw)), t.text());
                        if focused {
                            cursor = Some((cx, (tail.len() as u16) + cy));
                        }
                        tail.extend(lines);
                    }
                }
                Row::Skip => {
                    tail.push(Line::from(Span::styled(
                        format!(
                            "{marker}{} skip this question  s",
                            if s.skipped { "☑" } else { "⊘" }
                        ),
                        if focused {
                            base
                        } else {
                            Style::default().fg(t.fog)
                        },
                    )));
                }
            }
            if focused {
                // The focused row washes the full width, like hover.
                for line in &mut tail[first..] {
                    let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
                    if w < inner_w {
                        line.spans.push(Span::styled(" ".repeat(inner_w - w), base));
                    }
                    for span in &mut line.spans {
                        span.style = span.style.bg(t.select_bg);
                    }
                }
            }
            row_spans.push((k, first, tail.len()));
        }
        // Action chips.
        tail.push(Line::default());
        let acts = self.acts();
        let mut act_cells = vec![];
        let mut spans = vec![];
        let mut x = 0u16;
        for (k, (label, a)) in acts.iter().enumerate() {
            let focused = self.focus == rows.len() + k;
            let style = if focused {
                ui::chip_style(Style::default(), true)
            } else if *a == ACT_SUBMIT {
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                t.muted()
            };
            let label = truncate(label, inner_w);
            let w = label.width() as u16;
            if x > 0 && usize::from(x + w) > inner_w {
                tail.push(Line::from(std::mem::take(&mut spans)));
                x = 0;
            }
            act_cells.push((tail.len(), x, w, *a));
            spans.push(Span::styled(label, style));
            spans.push(Span::raw("  "));
            x += w + 2;
        }
        tail.push(Line::from(spans));
        tail.push(Line::from(Span::styled(
            truncate(
                if self.items.len() > 1 {
                    "1-9 pick · ↑↓ move · ←→/tab question · s skip · ctrl+s submit · esc later"
                } else {
                    "1-9 pick · ↑↓ move · enter choose · s skip · ctrl+s submit · esc later"
                },
                inner_w,
            ),
            t.faint(),
        )));
        // Size: head, body (clipped), tail.
        let max_body = (screen.height as usize)
            .saturating_sub(head.len() + tail.len() + 4)
            .max(3);
        let view = body.len().min(max_body);
        self.body_len = body.len();
        self.body_view = view;
        self.body_top = self.body_top.min(body.len().saturating_sub(view));
        let mut lines = head.clone();
        let body_start = lines.len();
        lines.extend(body.into_iter().skip(self.body_top).take(view));
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
        for (x, w, i) in tab_cells {
            hits.add(
                Rect {
                    x: inner.x + x,
                    y: inner.y,
                    width: w,
                    height: 1,
                },
                Target::Tab(i),
            );
        }
        for (k, a, b) in row_spans {
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
                Target::Row(k),
            );
        }
        for (row, x, w, a) in act_cells {
            let ay = inner.y + (tail_start + row) as u16;
            if ay < inner.bottom() {
                hits.add(
                    Rect {
                        x: inner.x + x,
                        y: ay,
                        width: w,
                        height: 1,
                    },
                    Target::Act(a),
                );
            }
        }
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::Overlay,
            Rect {
                x: area.right().saturating_sub(2),
                y: inner.y + body_start as u16,
                width: 1,
                height: view as u16,
            },
            self.body_len,
            view,
            self.body_top,
        );
        if let Some((x, y)) = cursor {
            let y = inner.y + tail_start as u16 + y;
            if y < inner.bottom() {
                frame.set_cursor_position((inner.x + x, y));
            }
        }
    }
}
