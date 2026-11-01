//! "Chat about this in a fork" (D64): an interactive hidden fork of a
//! question's asker. The view shows the questions, the fork's chat and an
//! input; the fork answers with its `answer_question` script function,
//! which closes it. The asker keeps running or waiting meanwhile.
use crate::composer::TextBuf;
use crate::engine::{ForkChatMessage, QItem, ToolCall, ToolResultData};
use crate::theme::theme;
use crate::transcript::{Kind, Level, SPINNER, Transcript};
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
    Send(String),
}

pub struct ForkChat {
    pub asker: String,
    /// A `/btw` side chat: no question, never closes by itself.
    pub side: bool,
    /// The fork's live status (`queued:N`, `running`; empty when idle).
    pub status: String,
    /// The fork answered (or the question was answered): a read-only record.
    pub closed: bool,
    /// The questions, one line each.
    pub summary: Vec<String>,
    /// The engine's fork id once open.
    pub fork: Option<String>,
    pub transcript: Transcript,
    pub input: TextBuf,
    /// Rows scrolled up from the bottom.
    back: usize,
}

impl ForkChat {
    pub fn new(asker: String, items: &[QItem]) -> Self {
        let summary = items
            .iter()
            .map(|q| q.text.lines().next().unwrap_or_default().replace("**", ""))
            .collect();
        Self::with_summary(asker, summary)
    }

    /// A fork reopened from its "answered via fork" row: the questions are
    /// gone, the answer summary heads the view.
    pub fn with_summary(asker: String, summary: Vec<String>) -> Self {
        Self {
            asker,
            side: false,
            status: String::new(),
            closed: false,
            summary,
            fork: None,
            transcript: Transcript::default(),
            input: TextBuf::default(),
            back: 0,
        }
    }

    pub fn notice(&mut self, level: Level, text: impl Into<String>) {
        self.transcript.notice(level, text);
    }

    /// One message of the fork's chat (live event).
    pub fn message(&mut self, role: &str, text: String) {
        match role {
            "user" => self.transcript.push(Kind::User(text)),
            "assistant" => self.transcript.push(Kind::Assistant(text)),
            _ => self.transcript.notice(Level::Info, text),
        }
        self.back = 0;
    }

    /// A fork tool call started: a live, timed row like the main agent's.
    pub fn tool_start(&mut self, call: Option<ToolCall>) {
        if let Some(c) = call {
            self.transcript.tool_start(c.id, c.name, c.arguments);
            self.back = 0;
        }
    }

    /// A fork tool call finished: its row gets the result and run time.
    /// Without structured data (older engines) the "name: output" text
    /// becomes an untimed row.
    pub fn tool(&mut self, text: String, call: Option<ToolCall>, result: Option<ToolResultData>) {
        match (call, result) {
            (Some(c), Some(r)) => {
                let known = self
                    .transcript
                    .entries
                    .iter()
                    .any(|e| matches!(&e.kind, Kind::Tool(t) if t.id == c.id));
                if known {
                    self.transcript
                        .tool_end(&c.id, r.output, r.is_error, r.details);
                } else {
                    self.transcript.recorded_tool(c, Some(r));
                }
            }
            (call, _) => {
                let (name, output) = text
                    .split_once(": ")
                    .map_or(("tool", text.as_str()), |(n, o)| (n, o));
                let id = call.map(|c| c.id).unwrap_or_default();
                self.transcript
                    .replayed_tool(id.clone(), name.to_string(), serde_json::json!({}));
                self.transcript.tool_end(
                    &id,
                    output.to_string(),
                    output.starts_with("Error:"),
                    Default::default(),
                );
            }
        }
        self.back = 0;
    }

    /// `opened`: the saved side record replaces whatever the view shows.
    /// Tool records come back as rows with arguments, result and run time;
    /// a call interrupted before its result shows as not recorded.
    pub fn load(&mut self, messages: Vec<ForkChatMessage>) {
        self.transcript.clear();
        for m in messages {
            match (m.role.as_str(), m.tool_call) {
                ("tool", Some(call)) => self.transcript.recorded_tool(call, m.tool_result),
                ("tool", None) => self.tool(m.text, None, None),
                (role, _) => {
                    self.message(role, m.text);
                    self.transcript.stamp_last(&m.ts);
                }
            }
        }
        self.transcript.abandon_running();
        self.transcript.seal();
        self.transcript.restamp();
        self.back = 0;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('c') if ctrl => return Action::Close,
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.input.insert("\n")
            }
            KeyCode::Enter if self.closed => {}
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input.text);
                self.input.clear();
                if !text.trim().is_empty() {
                    self.back = 0;
                    return Action::Send(text);
                }
            }
            KeyCode::Char(c) if !ctrl => self.input.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::PageUp | KeyCode::Up => self.back += 3,
            KeyCode::PageDown | KeyCode::Down => self.back = self.back.saturating_sub(3),
            _ => {}
        }
        Action::None
    }

    pub fn wheel(&mut self, delta: isize) {
        self.back = (self.back as isize - delta).max(0) as usize;
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits, tick: usize) {
        let t = theme();
        let area = Rect {
            x: screen.x + 2,
            y: screen.y + 1,
            width: screen.width.saturating_sub(4),
            height: screen.height.saturating_sub(2),
        };
        let inner_w = area.width.saturating_sub(5) as usize;
        let mut head: Vec<Line<'static>> = self
            .summary
            .iter()
            .map(|q| {
                Line::from(vec![
                    Span::styled("? ", Style::default().fg(t.yellow)),
                    Span::styled(truncate(q, inner_w.saturating_sub(2)), t.bold()),
                ])
            })
            .collect();
        let about = if self.side {
            format!(
                "a side chat: a private fork of {} · read-only tools and web · nothing here enters any agent's context · kept for this session (/btw reopens it; after a compaction a fresh one starts and can look up the earlier ones)",
                self.asker
            )
        } else {
            format!(
                "a private fork of {} · read-only tools and web · it answers with answer_question, which closes it · nothing here enters any agent's context",
                self.asker
            )
        };
        head.extend(crate::wrap::wrap_lines(
            vec![Line::from(Span::styled(about, t.faint()))],
            inner_w,
            &[],
            &[],
        ));
        head.push(Line::from(Span::styled(
            "─".repeat(inner_w),
            Style::default().fg(t.rule),
        )));
        // Chat rows.
        let mut rows: Vec<Line<'static>> = vec![];
        let n = self.transcript.entries.len();
        for i in 0..n {
            if i > 0 && self.transcript.gap_before(i) {
                rows.push(Line::default());
            }
            rows.extend(
                self.transcript.entries[i]
                    .render(inner_w, tick)
                    .lines
                    .clone(),
            );
        }
        if rows.is_empty() {
            rows.push(Line::from(Span::styled(
                if self.side {
                    "ask anything on the side; it sees this agent's context, can read files and search the web"
                } else {
                    "ask the fork about the question; it can read files and search the web"
                },
                t.faint(),
            )));
        }
        let (input_rows, (cx, cy)) = self.input.layout(
            inner_w,
            Span::styled("› ", Style::default().fg(t.accent)),
            Span::raw("  "),
            t.text(),
        );
        let input_rows: Vec<Line<'static>> = if self.input.text.is_empty() || self.closed {
            let hint = if self.closed {
                "this fork answered the question · a read-only record · esc close"
            } else if self.side {
                "message the side chat · enter send · esc close (/btw reopens it)"
            } else {
                "message the fork · enter send · esc close (the question stays pending)"
            };
            vec![Line::from(vec![
                Span::styled("› ", Style::default().fg(t.accent)),
                Span::styled(hint, t.faint()),
            ])]
        } else {
            input_rows
        };
        let body_h = (area.height as usize)
            .saturating_sub(2 + head.len() + input_rows.len() + 1)
            .max(1);
        let total = rows.len();
        let max_back = total.saturating_sub(body_h);
        self.back = self.back.min(max_back);
        let top = total.saturating_sub(body_h + self.back);
        let mut lines = head.clone();
        lines.extend(rows.into_iter().skip(top).take(body_h));
        while lines.len() < head.len() + body_h {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            "─".repeat(inner_w),
            Style::default().fg(t.rule),
        )));
        let input_y = lines.len();
        lines.extend(input_rows);
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        let state = if self.fork.is_none() {
            " · not connected".to_string()
        } else if self.closed {
            " · answered".to_string()
        } else if !self.status.is_empty() {
            format!(
                " · {} {}",
                SPINNER[tick % SPINNER.len()],
                crate::engine::status_text(&self.status)
            )
        } else {
            String::new()
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .title(Span::styled(
                if self.side {
                    format!(" Side chat (/btw) · {}{state} ", self.asker)
                } else {
                    format!(" Chat about this in a fork · {}{state} ", self.asker)
                },
                t.bold(),
            ))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(t.surface));
        frame.render_widget(Paragraph::new(lines).block(block), area);
        let inner_y = area.y + 1;
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::Overlay,
            Rect {
                x: area.right().saturating_sub(2),
                y: inner_y + head.len() as u16,
                width: 1,
                height: body_h as u16,
            },
            total,
            body_h,
            top,
        );
        let cursor = if self.input.text.is_empty() || self.closed {
            (2, 0)
        } else {
            (cx, cy)
        };
        frame.set_cursor_position((area.x + 2 + cursor.0, inner_y + input_y as u16 + cursor.1));
    }
}
