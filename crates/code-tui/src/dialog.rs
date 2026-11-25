//! Reusable modal dialog: title, a markdown or file diff body, a list of options
//! and an optional free-text field. Adapted from FlyCockpit's Cockpit dialog state
//! machine reduced to one page; approvals, questions and the session-lock
//! dialog will reuse it.
use crate::textbuf::TextBuf;
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
    /// Destructive or approving (see [`DialogOption::guarded`]).
    pub guarded: bool,
}

impl DialogOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            key: None,
            guarded: false,
        }
    }
    /// Destructive or approving (compact, delete, discard, allow, submit
    /// and the like): typed text never chooses it. A bare key or number
    /// only shows [`deliberate_hint`]; alt+key, or Enter (or a click)
    /// after moving the selection with ↑/↓, chooses it.
    pub fn guarded(mut self) -> Self {
        self.guarded = true;
        self
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
    title: String,
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
    guard: TypeAhead,
    /// False until the user moves the selection (↑/↓) or clicks: typing
    /// keys never choose an option before that (a dialog shown at start-up
    /// can meet keys typed without looking).
    armed: bool,
    /// A typing key was ignored while unarmed: the footer says why.
    nudged: bool,
    /// The user moved the selection with ↑/↓ (or alt+key, a click): only
    /// then can Enter choose a guarded option.
    moved: bool,
    /// The user put the cursor on an option (↑/↓ in the list, a click):
    /// until then it rests where the dialog opened it.
    steered: bool,
    /// A bare key met a guarded option: the footer says how to choose it.
    hint: Option<String>,
}

/// `title` becomes `first` (taken from `title` while empty) and how many
/// prompts wait behind it.
pub(crate) fn retitle(title: &mut String, first: &mut String, waiting: usize) {
    if first.is_empty() {
        *first = title.clone();
    }
    *title = if waiting > 0 {
        format!("{first} · {waiting} more waiting")
    } else {
        first.clone()
    };
}

/// The pause that ends a typing burst which ran into a new dialog.
const TYPE_AHEAD: Duration = Duration::from_millis(400);

/// How to choose a guarded option whose shortcut is `key` (a letter or
/// its number).
pub(crate) fn deliberate_hint(key: &str) -> String {
    format!("alt+{key} or ↑↓ + Enter")
}

/// The opening grace shared by every dialog: keys typed ahead (a dialog
/// that opened while the user was typing) are swallowed until a pause of
/// [`TYPE_AHEAD`]; each swallowed key extends it.
pub(crate) struct TypeAhead(Option<Instant>);

impl TypeAhead {
    pub(crate) fn opened_at(now: Instant) -> Self {
        Self(Some(now + TYPE_AHEAD))
    }
    /// Whether `key` at `now` belongs to the typing burst.
    pub(crate) fn swallows(&mut self, key: &KeyEvent, now: Instant) -> bool {
        let typing = matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace
        ) && key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        match self.0 {
            Some(until) if typing && now < until => {
                self.0 = Some(now + TYPE_AHEAD);
                true
            }
            _ => false,
        }
    }
    /// A deliberate step (a click, an arrow, alt+key) ends the grace at once.
    pub(crate) fn end(&mut self) {
        self.0 = None;
    }
}

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
            guard: TypeAhead::opened_at(Instant::now()),
            armed: true,
            nudged: false,
            moved: false,
            steered: false,
            hint: None,
        }
        .start_safe()
    }
    /// A dialog opens with the cursor on its first safe option, so a plain
    /// Enter does the safe thing at once; a guarded one takes ↑↓ + Enter
    /// or its alt+key (when every option is guarded, Enter only hints).
    fn start_safe(mut self) -> Self {
        if self.is_guarded(self.selected)
            && let Some(i) = self.options.iter().position(|o| !o.guarded)
        {
            self.selected = i;
        }
        self
    }
    /// A prompt dialog's title says how many prompts wait behind it.
    /// `first` keeps the prompt's own title across renders (the caller
    /// clears it for a new prompt; empty: this dialog's title).
    pub fn show_waiting(&mut self, first: &mut String, waiting: usize) {
        retitle(&mut self.title, first, waiting);
    }
    /// A paste goes into the text field (one line).
    pub fn paste(&mut self, text: &str) {
        if let Some((_, buf)) = &mut self.text {
            buf.insert(&text.replace('\n', " "));
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
        self.start_safe()
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
            .is_some_and(|(_, b)| b.text().trim().is_empty())
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
            .map(|(_, b)| b.text().to_string())
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
        self.moved = true;
        self.steered = true;
        self.guard.end();
        self.chosen(i)
    }
    fn is_guarded(&self, idx: usize) -> bool {
        self.options.get(idx).is_some_and(|o| o.guarded)
    }
    /// Whether a plain Enter may choose option `idx`: a guarded one only
    /// once the user moved the selection onto it.
    fn enter_chooses(&self, idx: usize) -> bool {
        !self.is_guarded(idx) || (self.moved && idx == self.selected)
    }
    /// The key that chooses option `idx` with alt: its letter, or its
    /// number. The one name every row, field hint and footer shows.
    fn alt_key(&self, idx: usize) -> String {
        self.options[idx]
            .key
            .map_or_else(|| (idx + 1).to_string(), String::from)
    }
    /// Typed text met guarded option `idx`: say how to choose it.
    fn refuse(&mut self, idx: usize) -> Outcome {
        self.hint = Some(format!(
            "{}: {}",
            self.options[idx].label.to_lowercase(),
            deliberate_hint(&self.alt_key(idx))
        ));
        Outcome::Pending
    }
    /// The option Enter in the text field chooses: the selected one when
    /// it takes the text, else the field's own option, else (while the
    /// cursor still rests on the safe option it opened on, which may
    /// ignore the field) the first that needs text, else the selected one.
    fn field_target(&self) -> usize {
        if self.needs_text.is_empty() || !self.wants_text(self.selected) {
            self.text_option
                .as_ref()
                .and_then(|id| self.options.iter().position(|o| &o.id == id))
                .or_else(|| {
                    self.options
                        .iter()
                        .position(|o| !self.steered && self.needs_text.contains(&o.id))
                })
                .unwrap_or(self.selected)
        } else {
            self.selected
        }
    }
    /// The option key `c` (bare or with alt) is for: its key, or its number.
    fn key_target(&self, c: char) -> Option<usize> {
        self.options
            .iter()
            .position(|o| o.key == Some(c))
            .or_else(|| {
                c.to_digit(10)
                    .map(|d| d as usize)
                    .filter(|d| (1..=self.options.len()).contains(d))
                    .map(|d| d - 1)
            })
    }
    /// The wheel scrolls a long body.
    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.body_top as isize + delta).max(0) as usize);
    }
    pub fn set_offset(&mut self, top: usize) {
        self.body_top = top.min(self.body_len.saturating_sub(self.body_view));
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        self.key_at(key, Instant::now())
    }

    /// One key at `now`. Typed text (letters, digits, space, Enter) never
    /// chooses a guarded option: only alt+key, or Enter after ↑/↓ does.
    pub(crate) fn key_at(&mut self, key: KeyEvent, now: Instant) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT) && !ctrl;
        // Typed-ahead keys (characters, Enter, Tab, Backspace) right after
        // the dialog opened belong to whatever the user was typing.
        if self.guard.swallows(&key, now) {
            return Outcome::Pending;
        }
        if !self.armed {
            match key.code {
                KeyCode::Up | KeyCode::Down => self.armed = true,
                KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace
                    if !ctrl && !alt =>
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
        if key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && ctrl) {
            return Outcome::Cancelled;
        }
        if key.code == KeyCode::Tab && self.text.is_some() {
            self.text_focus = !self.text_focus;
            return Outcome::Pending;
        }
        // alt+Enter is as deliberate as an arrow.
        if alt && key.code == KeyCode::Enter {
            self.moved = true;
            self.guard.end();
        }
        // alt+key (or alt+number) chooses its option from anywhere, the
        // text field too: from the field it takes the field as it is (an
        // empty optional note, a prefill), as Enter there would.
        if alt
            && let KeyCode::Char(c) = key.code
            && let Some(i) = self.key_target(c)
        {
            self.moved = true;
            self.guard.end();
            return self.chosen(i);
        }
        if self.text_focus
            && let Some((_, buf)) = &mut self.text
        {
            match key.code {
                KeyCode::Enter => {
                    let idx = self.field_target();
                    if !alt && !self.enter_chooses(idx) {
                        return self.refuse(idx);
                    }
                    return self.chosen(idx);
                }
                KeyCode::Char('u') if ctrl => buf.kill_line(),
                KeyCode::Char('w') if ctrl => buf.delete_word(),
                KeyCode::Char(c) if !alt => buf.insert(c.encode_utf8(&mut [0; 4])),
                KeyCode::Backspace => buf.backspace(),
                KeyCode::Delete => buf.delete(),
                KeyCode::Left => buf.left(),
                KeyCode::Right => buf.right(),
                KeyCode::Home => buf.home(),
                KeyCode::End => buf.end(),
                KeyCode::Up | KeyCode::Down => {
                    self.text_focus = false;
                    self.moved = true;
                    self.guard.end();
                }
                _ => {}
            }
            return Outcome::Pending;
        }
        let n = self.options.len().max(1);
        match key.code {
            KeyCode::Up => {
                self.selected = (self.selected + n - 1) % n;
                self.moved = true;
                self.steered = true;
                self.guard.end();
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1) % n;
                self.moved = true;
                self.steered = true;
                self.guard.end();
            }
            KeyCode::Enter => {
                if !alt && !self.enter_chooses(self.selected) {
                    return self.refuse(self.selected);
                }
                return self.chosen(self.selected);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if let Some(i) = self.key_target(c) {
                    if self.is_guarded(i) {
                        return self.refuse(i);
                    }
                    return self.chosen(i);
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
            let key = match (o.key, o.guarded) {
                (_, true) => format!("  alt+{}", self.alt_key(i)),
                (Some(k), false) => format!("  {k}"),
                (None, false) => String::new(),
            };
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
            if buf.text().is_empty() {
                // Focused: say what Enter will do.
                let hint = if self.text_focus {
                    let idx = self.field_target();
                    let target = self
                        .options
                        .get(idx)
                        .map(|o| o.label.to_lowercase())
                        .unwrap_or_default();
                    let enter = if self.enter_chooses(idx) {
                        "enter".to_string()
                    } else {
                        format!("alt+{}", self.alt_key(idx))
                    };
                    format!("{placeholder} · {enter}: {target} · tab: back to the options")
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
        if let Some(hint) = &self.hint {
            tail.push(Line::default());
            tail.push(Line::from(Span::styled(
                crate::wrap::truncate(hint, inner_w),
                Style::default().fg(t.yellow),
            )));
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small deterministic generator (no test dependency needed).
    pub(crate) struct Rng(u64);
    impl Rng {
        pub(crate) fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }
        pub(crate) fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
        pub(crate) fn chance(&mut self, percent: usize) -> bool {
            self.below(100) < percent
        }
    }

    /// One key of typed prose: letters, digits, space, punctuation,
    /// Backspace or Enter, with no modifier but shift.
    pub(crate) fn prose_key(rng: &mut Rng) -> KeyEvent {
        const CHARS: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789     .,?!'-";
        match rng.below(12) {
            0 => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            1 if rng.chance(30) => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            _ => {
                let c = CHARS[rng.below(CHARS.len())] as char;
                let m = if c.is_ascii_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                KeyEvent::new(KeyCode::Char(c), m)
            }
        }
    }

    /// Time between two typed keys: mostly a burst, sometimes a pause long
    /// enough to end the opening grace.
    pub(crate) fn gap(rng: &mut Rng) -> Duration {
        Duration::from_millis(match rng.below(4) {
            0 => 400 + rng.below(1600) as u64,
            _ => rng.below(250) as u64,
        })
    }

    /// Feeds `keys` random prose keys into `d` (opened at `open`); the
    /// dialog stays open after a choice (as "jump" keeps it), and no
    /// guarded option may ever be chosen.
    pub(crate) fn assert_prose_is_safe(d: &mut Dialog, rng: &mut Rng, keys: usize) {
        let mut now = Instant::now();
        d.guard = TypeAhead::opened_at(now);
        let mut typed = String::new();
        for _ in 0..keys {
            now += gap(rng);
            let key = prose_key(rng);
            typed.push_str(&match key.code {
                KeyCode::Char(c) => c.to_string(),
                KeyCode::Enter => "⏎".into(),
                _ => "⌫".into(),
            });
            if let Outcome::Chosen { id, .. } = d.key_at(key, now) {
                let o = d.options.iter().find(|o| o.id == id).unwrap();
                assert!(!o.guarded, "typed {typed:?} chose guarded {id:?}");
            }
        }
    }

    fn at(open: Instant, ms: u64) -> Instant {
        open + Duration::from_millis(ms)
    }
    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }
    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    /// `d` opened at `open` (tests drive the clock).
    fn opened(mut d: Dialog, open: Instant) -> Dialog {
        d.guard = TypeAhead::opened_at(open);
        d
    }
    fn chosen(o: Outcome) -> Option<String> {
        match o {
            Outcome::Chosen { id, .. } => Some(id),
            _ => None,
        }
    }
    /// Types `text` one key every `step` ms from `from`; the ids chosen.
    fn type_text(d: &mut Dialog, open: Instant, from: u64, step: u64, text: &str) -> Vec<String> {
        text.chars()
            .enumerate()
            .filter_map(|(i, c)| {
                let key = if c == '\n' {
                    code(KeyCode::Enter)
                } else {
                    k(c)
                };
                chosen(d.key_at(key, at(open, from + step * i as u64)))
            })
            .collect()
    }

    /// The "Switch model?" dialog's options.
    fn switch_model() -> Dialog {
        Dialog::new(
            "Switch model?",
            Body::None,
            vec![
                DialogOption::new("keep", "Keep m").key('k'),
                DialogOption::new("compact", "Compact with m, then switch")
                    .key('c')
                    .guarded(),
                DialogOption::new("switch", "Switch anyway").key('s'),
            ],
        )
    }
    /// "Stop the branch": the required reason field opens focused.
    fn stop_branch() -> Dialog {
        Dialog::new(
            "Stop",
            Body::None,
            vec![
                DialogOption::new("cancel", "Keep it running"),
                DialogOption::new("stop", "Stop the branch")
                    .key('s')
                    .guarded(),
            ],
        )
        .with_text("reason (required)")
        .needs_text(&["stop"])
        .text_option("stop", true)
    }
    /// A tool approval (a child's: with "Jump to agent").
    fn approval(child: bool) -> Dialog {
        let mut options = vec![
            DialogOption::new("once", "Allow once").key('o').guarded(),
            DialogOption::new("session", "Allow for session")
                .key('s')
                .guarded(),
            DialogOption::new("deny", "Deny with feedback").key('d'),
        ];
        if child {
            options.push(DialogOption::new("jump", "Jump to agent").key('j'));
        }
        Dialog::new("Allow bash?", Body::None, options)
            .with_text("feedback for the model")
            .text_option("deny", false)
    }

    #[test]
    fn the_brief_repro_compacts_nothing() {
        // "Switch model?" opens in the middle of typing "which is it", and
        // again after a pause: compact never fires.
        for from in [0, 1000] {
            let open = Instant::now();
            let mut d = opened(switch_model(), open);
            let ids = type_text(&mut d, open, from, 60, "which is it\n");
            assert!(!ids.contains(&"compact".to_string()), "{ids:?}");
        }
        let open = Instant::now();
        let mut d = opened(switch_model(), open);
        // In the burst: swallowed, nothing chosen.
        assert!(type_text(&mut d, open, 0, 60, "which is it").is_empty());
        // After a pause a bare c only shows how to compact.
        assert!(matches!(d.key_at(k('c'), at(open, 2000)), Outcome::Pending));
        assert_eq!(
            d.hint.as_deref(),
            Some("compact with m, then switch: alt+c or ↑↓ + Enter")
        );
        assert!(matches!(d.key_at(k('2'), at(open, 2100)), Outcome::Pending));
        assert!(chosen(d.key_at(code(KeyCode::Enter), at(open, 2200))).as_deref() == Some("keep"));
    }

    #[test]
    fn job_then_enter_in_a_tool_approval_allows_nothing() {
        for child in [false, true] {
            for step in [50, 500] {
                let open = Instant::now();
                let mut d = opened(approval(child), open);
                let ids = type_text(&mut d, open, 1000, step, "job\n");
                assert!(
                    ids.iter().all(|id| id == "jump"),
                    "child {child}, step {step}: {ids:?}"
                );
                // The bare o only hinted; Enter on the opening selection
                // (Deny, the first safe option) asks for the feedback.
                assert!(d.hint.as_deref().unwrap().contains("alt+o or ↑↓ + Enter"));
                assert!(d.selected == 2 && d.text_focus);
            }
        }
    }

    #[test]
    fn j_and_k_do_not_move() {
        let open = Instant::now();
        let mut d = opened(approval(false), open);
        for c in "jjkkjj".chars() {
            d.key_at(k(c), at(open, 1000));
        }
        assert_eq!(d.selected, 2);
        d.key_at(code(KeyCode::Up), at(open, 1000));
        assert_eq!(d.selected, 1);
        d.key_at(code(KeyCode::Down), at(open, 1000));
        assert_eq!(d.selected, 2);
    }

    #[test]
    fn alt_key_or_arrows_and_enter_choose_a_guarded_option() {
        let open = Instant::now();
        // alt+key, even inside the opening grace.
        let mut d = opened(switch_model(), open);
        assert_eq!(
            chosen(d.key_at(alt('c'), at(open, 10))).as_deref(),
            Some("compact")
        );
        // alt+number.
        let mut d = opened(switch_model(), open);
        assert_eq!(
            chosen(d.key_at(alt('2'), at(open, 1000))).as_deref(),
            Some("compact")
        );
        // ↓ then Enter.
        let mut d = opened(switch_model(), open);
        d.key_at(code(KeyCode::Down), at(open, 1000));
        assert_eq!(
            chosen(d.key_at(code(KeyCode::Enter), at(open, 1100))).as_deref(),
            Some("compact")
        );
        // From the opening Deny, ↓ (wrapping) onto Allow once, then Enter.
        let mut d = opened(approval(false), open);
        d.key_at(code(KeyCode::Down), at(open, 1000));
        assert_eq!(
            chosen(d.key_at(code(KeyCode::Enter), at(open, 1100))).as_deref(),
            Some("once")
        );
        // A click chooses at once, inside the grace too.
        let mut d = opened(approval(false), open);
        assert_eq!(chosen(d.click(1)).as_deref(), Some("session"));
        // Bare keys still choose the others.
        let mut d = opened(switch_model(), open);
        assert_eq!(
            chosen(d.key_at(k('s'), at(open, 1000))).as_deref(),
            Some("switch")
        );
        let mut d = opened(switch_model(), open);
        assert_eq!(
            chosen(d.key_at(k('3'), at(open, 1000))).as_deref(),
            Some("switch")
        );
    }

    #[test]
    fn a_field_whose_enter_is_guarded_needs_alt_or_arrows() {
        // "Close terminal": the note field is focused and Enter would close
        // and send.
        let close = || {
            Dialog::new(
                "Close terminal?",
                Body::None,
                vec![
                    DialogOption::new("close", "Close and send")
                        .key('c')
                        .guarded(),
                    DialogOption::new("stay", "Keep it open"),
                ],
            )
            .with_text("note")
            .text_option("close", true)
        };
        let open = Instant::now();
        let mut d = opened(close(), open);
        assert!(type_text(&mut d, open, 1000, 50, "done here\n").is_empty());
        assert_eq!(
            d.hint.as_deref(),
            Some("close and send: alt+c or ↑↓ + Enter")
        );
        // alt+c, alt+1 and alt+Enter (from the field) all close and send.
        for key in [alt('c'), alt('1')] {
            let mut d = opened(close(), open);
            type_text(&mut d, open, 1000, 50, "done here");
            assert!(matches!(
                d.key_at(key, at(open, 2000)),
                Outcome::Chosen { id, text: Some(t) } if id == "close" && t == "done here"
            ));
        }
        assert!(matches!(
            d.key_at(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), at(open, 2000)),
            Outcome::Chosen { id, text: Some(t) } if id == "close" && t == "done here"
        ));
        // ↑ leaves the field on the opening Keep it open; ↑ again onto
        // Close and send, then Enter.
        let mut d = opened(close(), open);
        type_text(&mut d, open, 1000, 50, "ok");
        d.key_at(code(KeyCode::Up), at(open, 2000));
        assert_eq!(
            chosen(d.key_at(code(KeyCode::Enter), at(open, 2050))).as_deref(),
            Some("stay")
        );
        let mut d = opened(close(), open);
        type_text(&mut d, open, 1000, 50, "ok");
        d.key_at(code(KeyCode::Up), at(open, 2000));
        d.key_at(code(KeyCode::Up), at(open, 2000));
        assert_eq!(
            chosen(d.key_at(code(KeyCode::Enter), at(open, 2100))).as_deref(),
            Some("close")
        );
        // "Stop the branch": the reason field opens focused on a safe
        // selection; ↓ out and tab back does not make Enter stop it.
        let mut d = opened(stop_branch(), open);
        // ↓ leaves the field, ↓↑ moves away and back.
        d.key_at(code(KeyCode::Down), at(open, 1000));
        d.key_at(code(KeyCode::Down), at(open, 1000));
        d.key_at(code(KeyCode::Up), at(open, 1000));
        d.key_at(code(KeyCode::Tab), at(open, 1000));
        assert!(d.text_focus && d.selected == 0);
        assert!(type_text(&mut d, open, 1100, 50, "stuck\n").is_empty());
        assert_eq!(
            d.hint.as_deref(),
            Some("stop the branch: alt+s or ↑↓ + Enter")
        );
        // ↓ out of the field and onto it; Enter takes the typed reason.
        d.key_at(code(KeyCode::Down), at(open, 2000));
        d.key_at(code(KeyCode::Down), at(open, 2000));
        assert_eq!(d.selected, 1);
        assert!(matches!(
            d.key_at(code(KeyCode::Enter), at(open, 2100)),
            Outcome::Chosen { id, text: Some(t) } if id == "stop" && t == "stuck"
        ));
        // A new "Stop the branch": alt+Enter on the empty required field
        // keeps it open; the reason typed at once and Enter stop.
        let mut d = opened(stop_branch(), open);
        assert!(matches!(
            d.key_at(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
                at(open, 10)
            ),
            Outcome::Pending
        ));
        assert!(d.text_focus && d.selected == 1);
        type_text(&mut d, open, 20, 10, "stuck");
        assert!(matches!(
            d.key_at(code(KeyCode::Enter), at(open, 100)),
            Outcome::Chosen { id, text: Some(t) } if id == "stop" && t == "stuck"
        ));
        // Roadblock: the message goes with Pivot only after choosing it
        // deliberately.
        let roadblock = Dialog::new(
            "Roadblock",
            Body::None,
            vec![
                DialogOption::new("pivot", "Pivot with a message")
                    .key('p')
                    .guarded(),
                DialogOption::new("pivot-edit", "Pivot: edit the contract…").key('e'),
                DialogOption::new("override", "Override").key('o').guarded(),
                DialogOption::new("stop", "Stop").key('s').guarded(),
            ],
        )
        .with_text("message or override note")
        .needs_text(&["pivot", "override"])
        .select("pivot-edit");
        let mut d = opened(roadblock, open);
        // Inside the opening grace: alt+p ends it, the message counts.
        d.key_at(alt('p'), at(open, 10));
        assert!(d.text_focus);
        assert!(matches!(
            {
                type_text(&mut d, open, 20, 10, "go left");
                d.key_at(code(KeyCode::Enter), at(open, 100))
            },
            Outcome::Chosen { id, text: Some(t) } if id == "pivot" && t == "go left"
        ));
    }

    #[test]
    fn the_field_key_takes_the_field_as_it_is() {
        let open = Instant::now();
        // An empty optional note: alt+c closes at once.
        let mut d = opened(
            Dialog::new(
                "Close terminal?",
                Body::None,
                vec![
                    DialogOption::new("close", "Close and send")
                        .key('c')
                        .guarded(),
                    DialogOption::new("stay", "Keep it open"),
                ],
            )
            .with_text("note")
            .text_option("close", true),
            open,
        );
        assert!(matches!(
            d.key_at(alt('c'), at(open, 1000)),
            Outcome::Chosen { id, text: None } if id == "close"
        ));
        // A prefilled question: alt+a from the field sends it; from the
        // options it opens the field to edit first.
        let ask = || {
            Dialog::new(
                "Ask",
                Body::None,
                vec![
                    DialogOption::new("ask", "Ask").key('a').guarded(),
                    DialogOption::new("cancel", "Cancel"),
                ],
            )
            .with_text("question")
            .prefill("status?")
            .text_option("ask", true)
        };
        let mut d = opened(ask(), open);
        assert!(matches!(
            d.key_at(alt('a'), at(open, 1000)),
            Outcome::Chosen { id, text: Some(t) } if id == "ask" && t == "status?"
        ));
        let mut d = opened(ask(), open);
        d.key_at(code(KeyCode::Tab), at(open, 1000));
        assert!(matches!(
            d.key_at(alt('a'), at(open, 1100)),
            Outcome::Pending
        ));
        assert!(d.text_focus);
        // Roadblock opens on the safe "edit the contract": a message typed
        // in the field goes with Pivot (deliberately), never to the editor.
        let roadblock = || {
            Dialog::new(
                "Roadblock",
                Body::None,
                vec![
                    DialogOption::new("pivot", "Pivot with a message")
                        .key('p')
                        .guarded(),
                    DialogOption::new("pivot-edit", "Pivot: edit the contract…").key('e'),
                    DialogOption::new("override", "Override").key('o').guarded(),
                    DialogOption::new("stop", "Stop").key('s').guarded(),
                ],
            )
            .with_text("message or override note")
            .needs_text(&["pivot", "override"])
        };
        let mut d = opened(roadblock(), open);
        assert_eq!(d.options[d.selected].id, "pivot-edit");
        d.key_at(code(KeyCode::Tab), at(open, 1000));
        assert!(type_text(&mut d, open, 1100, 10, "go left\n").is_empty());
        assert_eq!(
            d.hint.as_deref(),
            Some("pivot with a message: alt+p or ↑↓ + Enter")
        );
        assert!(matches!(
            d.key_at(alt('p'), at(open, 1300)),
            Outcome::Chosen { id, text: Some(t) } if id == "pivot" && t == "go left"
        ));
        // alt+Enter from that field pivots with the message too.
        let mut d = opened(roadblock(), open);
        d.key_at(code(KeyCode::Tab), at(open, 1000));
        type_text(&mut d, open, 1100, 10, "go left");
        assert!(matches!(
            d.key_at(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), at(open, 1300)),
            Outcome::Chosen { id, text: Some(t) } if id == "pivot" && t == "go left"
        ));
        // Write outside scope opens on Reject; ↓ to Grant, Tab, directories
        // and Enter (or alt+Enter) grant them: the field goes with the
        // option the user moved to.
        for enter in [KeyModifiers::NONE, KeyModifiers::ALT] {
            let mut d = opened(
                Dialog::new(
                    "Write outside scope",
                    Body::None,
                    vec![
                        DialogOption::new("apply", "Apply").key('a').guarded(),
                        DialogOption::new("reject", "Reject with a reason").key('r'),
                        DialogOption::new("grant", "Grant scope").key('g').guarded(),
                    ],
                )
                .with_text("reason (reject) or directories (grant)")
                .needs_text(&["reject"]),
                open,
            );
            assert_eq!(d.options[d.selected].id, "reject");
            d.key_at(code(KeyCode::Down), at(open, 1000));
            d.key_at(code(KeyCode::Tab), at(open, 1000));
            type_text(&mut d, open, 1100, 10, "src");
            assert!(matches!(
                d.key_at(KeyEvent::new(KeyCode::Enter, enter), at(open, 1300)),
                Outcome::Chosen { id, text: Some(t) } if id == "grant" && t == "src"
            ));
        }
    }

    #[test]
    fn the_opening_grace_swallows_a_burst_and_a_click_ends_it() {
        let open = Instant::now();
        let mut d = opened(switch_model(), open);
        // Every key 100 ms after the last extends the grace.
        assert!(type_text(&mut d, open, 0, 100, "kkkkkkkk").is_empty());
        assert_eq!(
            chosen(d.key_at(k('k'), at(open, 1300))).as_deref(),
            Some("keep")
        );
        let mut d = opened(approval(false), open);
        d.click(2);
        assert!(d.text_focus);
        assert!(type_text(&mut d, open, 10, 10, "no").is_empty());
        assert!(matches!(
            d.key_at(code(KeyCode::Enter), at(open, 40)),
            Outcome::Chosen { id, text: Some(t) } if id == "deny" && t == "no"
        ));
    }

    #[test]
    fn an_arrow_ends_the_opening_grace() {
        let open = Instant::now();
        let mut d = opened(switch_model(), open);
        d.key_at(code(KeyCode::Down), at(open, 10));
        assert_eq!(
            chosen(d.key_at(code(KeyCode::Enter), at(open, 20))).as_deref(),
            Some("compact")
        );
        // ↓ out of a focused field and Tab back: typing counts at once.
        let mut d = opened(approval(false).text_option("deny", true), open);
        d.key_at(code(KeyCode::Down), at(open, 10));
        d.key_at(code(KeyCode::Down), at(open, 20));
        assert_eq!(chosen(d.key_at(k('d'), at(open, 30))).as_deref(), None);
        assert!(d.text_focus);
        type_text(&mut d, open, 40, 10, "no");
        assert!(matches!(
            d.key_at(code(KeyCode::Enter), at(open, 70)),
            Outcome::Chosen { id, text: Some(t) } if id == "deny" && t == "no"
        ));
    }

    /// Any dialog: random options, keys, guards, a text field and its
    /// targets, a prefill, a selection.
    fn random_dialog(rng: &mut Rng) -> Dialog {
        const KEYS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let n = 1 + rng.below(6);
        let options: Vec<DialogOption> = (0..n)
            .map(|i| {
                let mut o = DialogOption::new(format!("o{i}"), format!("Option {i}"));
                if rng.chance(70) {
                    o = o.key(KEYS[rng.below(KEYS.len())] as char);
                }
                if rng.chance(50) {
                    o = o.guarded();
                }
                o
            })
            .collect();
        let mut d = Dialog::new("t", Body::None, options);
        if rng.chance(60) {
            d = d.with_text("field");
            if rng.chance(70) {
                let target = format!("o{}", rng.below(n));
                d = d.text_option(target, rng.chance(50));
            }
            if rng.chance(40) {
                let ids: Vec<String> = (0..n)
                    .filter(|_| rng.chance(40))
                    .map(|i| format!("o{i}"))
                    .collect();
                let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                d = d.needs_text(&ids);
            }
            if rng.chance(30) {
                d = d.prefill("draft");
            }
        }
        d = d.select(&format!("o{}", rng.below(n)));
        if rng.chance(20) {
            d = d.require_focus();
        }
        d
    }

    /// A freshly opened dialog's cursor is on its first safe option
    /// (unless it chose a safe one, or none is safe), and a plain Enter
    /// there never fires a guarded option.
    pub(crate) fn assert_opens_safe(d: &mut Dialog) {
        let first_safe = d.options.iter().position(|o| !o.guarded);
        assert!(
            !d.is_guarded(d.selected) || first_safe.is_none(),
            "opened on guarded {:?}",
            d.options[d.selected].id
        );
        // After the opening grace, so the Enter counts.
        let open = Instant::now();
        d.guard = TypeAhead::opened_at(open);
        if let Outcome::Chosen { id, .. } = d.key_at(code(KeyCode::Enter), at(open, 1000)) {
            let o = d.options.iter().find(|o| o.id == id).unwrap();
            assert!(!o.guarded, "a fresh Enter chose guarded {id:?}");
        }
    }

    /// The dialog's rows as text, rendered on a 120×60 screen.
    fn rendered(d: &mut Dialog) -> Vec<String> {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 60)).unwrap();
        let mut hits = crate::ui::Hits::default();
        term.draw(|f| d.render(f, f.area(), &mut hits)).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Every `alt+X` in `line` names the key that chooses, with alt, the
    /// option whose label the line shows; alt+enter is never named.
    fn assert_line_agrees(d: &Dialog, line: &str) {
        let lower = line.to_lowercase();
        for (at, _) in lower.match_indices("alt+") {
            let name: String = lower[at + 4..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '<' || *c == '>')
                .collect();
            if name.is_empty() {
                continue; // cut off by the width
            }
            assert_ne!(name, "enter", "alt+enter named in {line:?}");
            let options: Vec<usize> = (0..d.options.len())
                .filter(|&i| lower.contains(&d.options[i].label.to_lowercase()))
                .collect();
            assert!(!options.is_empty(), "alt+{name} names no option: {line:?}");
            let key = name.chars().next().unwrap();
            assert!(
                name.chars().count() == 1
                    && d.key_target(key).is_some_and(|i| options.contains(&i))
                    && options.iter().any(|&i| d.alt_key(i) == name),
                "alt+{name} is not the one key of what {line:?} names"
            );
        }
    }

    /// The options, the field's placeholder and Enter hint, and the
    /// footer a refused key shows all name the same alt+key per guarded
    /// option, the one that chooses it.
    pub(crate) fn assert_keys_agree(d: &mut Dialog) {
        let open = Instant::now();
        d.guard = TypeAhead::opened_at(open);
        let mut lines = rendered(d);
        if let Some((placeholder, _)) = &d.text {
            lines.push(placeholder.clone());
            // The field's Enter hint shows only while it has focus.
            if !d.text_focus && d.armed {
                d.key_at(code(KeyCode::Tab), at(open, 1000));
                lines.extend(rendered(d));
            }
        }
        for line in &lines {
            assert_line_agrees(d, line);
        }
        for i in 0..d.options.len() {
            if !d.options[i].guarded {
                continue;
            }
            let row = format!("{}  alt+{}", d.options[i].label, d.alt_key(i));
            assert!(
                lines.iter().any(|l| l.contains(&row)),
                "no row {row:?} in {lines:#?}"
            );
            d.refuse(i);
            let hint = d.hint.take().unwrap();
            assert_line_agrees(d, &hint);
            assert!(hint.contains(&format!("alt+{} ", d.alt_key(i))), "{hint}");
        }
        // The key the focused field names does what it says, with the field
        // as it is (unless that option needs text and the field is empty).
        let idx = d.field_target();
        if d.text_focus && d.is_guarded(idx) {
            let id = d.options[idx].id.clone();
            let empty = d
                .text
                .as_ref()
                .is_some_and(|(_, b)| b.text().trim().is_empty());
            let key = d.alt_key(idx).chars().next().unwrap();
            let out = d.key_at(alt(key), at(open, 1100));
            if !(empty && d.needs_text.contains(&id)) {
                assert!(
                    matches!(&out, Outcome::Chosen { id: got, .. } if *got == id),
                    "alt+{key} from the field did not choose {id:?}"
                );
            }
        }
    }

    #[test]
    fn random_prose_never_chooses_a_guarded_option() {
        for seed in 0..3000 {
            let mut rng = Rng::new(seed);
            let mut d = random_dialog(&mut rng);
            assert_prose_is_safe(&mut d, &mut rng, 60);
        }
    }

    #[test]
    fn a_fresh_dialog_opens_on_a_safe_option_and_enter_does_it() {
        for seed in 0..3000 {
            let mut d = random_dialog(&mut Rng::new(seed));
            assert_opens_safe(&mut d);
        }
        // The first safe option, also over a guarded explicit selection.
        let quit = || {
            Dialog::new(
                "Quit?",
                Body::None,
                vec![
                    DialogOption::new("quit", "Quit").key('q').guarded(),
                    DialogOption::new("stay", "Stay").key('n'),
                ],
            )
        };
        let open = Instant::now();
        for d in [quit(), quit().select("quit")] {
            let mut d = opened(d, open);
            assert_eq!(d.selected, 1);
            assert_eq!(
                chosen(d.key_at(code(KeyCode::Enter), at(open, 1000))).as_deref(),
                Some("stay")
            );
        }
        // Quitting still takes ↓ + Enter or alt+q.
        let mut d = opened(quit(), open);
        d.key_at(code(KeyCode::Down), at(open, 1000));
        assert_eq!(
            chosen(d.key_at(code(KeyCode::Enter), at(open, 1100))).as_deref(),
            Some("quit")
        );
        let mut d = opened(quit(), open);
        assert_eq!(
            chosen(d.key_at(alt('q'), at(open, 1000))).as_deref(),
            Some("quit")
        );
        // Every option guarded: the cursor stays, Enter only hints.
        let mut d = opened(
            Dialog::new(
                "t",
                Body::None,
                vec![DialogOption::new("a", "A").key('a').guarded()],
            ),
            open,
        );
        assert!(matches!(
            d.key_at(code(KeyCode::Enter), at(open, 1000)),
            Outcome::Pending
        ));
        assert_eq!(d.hint.as_deref(), Some("a: alt+a or ↑↓ + Enter"));
    }
}
