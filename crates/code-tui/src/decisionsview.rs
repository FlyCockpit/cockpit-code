//! `/decisions` (D128): the ledger of choices agents made on the user's
//! behalf. Open records first (closed ones behind a toggle); each shows its
//! question, choice, alternatives, reason, deciding agent, affected files
//! and criteria, and time. Keep closes it, change sends steering to the
//! deciding agent, discuss opens a private side chat with it.
use crate::engine::DecisionView;
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

pub enum DecisionsAction {
    None,
    Close,
    Keep(String),
    Change(String),
    Discuss(String),
}

const ACT_CLOSED: u8 = 0;
const ACT_KEEP: u8 = 10;
const ACT_CHANGE: u8 = 11;
const ACT_DISCUSS: u8 = 12;

pub struct DecisionsView {
    all: Vec<DecisionView>,
    /// Agent display labels for the records' deciding agents.
    labels: Vec<(String, String)>,
    pub show_closed: bool,
    selected: Option<String>,
    top: usize,
    view: usize,
    pinned: bool,
    detail_top: usize,
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "approach" => "approach",
        "scope" => "scope answer",
        "proposal" => "criteria proposal",
        "roadblock" => "roadblock",
        "safety" => "safety review",
        "fallback" => "model fallback",
        "agent_stop" => "stopped agent",
        "agent_finalize" => "finalized agent",
        k => k,
    }
}

fn status_mark(status: &str) -> (&'static str, ratatui::style::Color) {
    let t = theme();
    match status {
        "open" => ("◇", t.yellow),
        "discussing" => ("◈", t.link),
        "kept" => ("✓", t.green),
        "changed" => ("↺", t.accent),
        _ => ("·", t.dim),
    }
}

impl DecisionsView {
    pub fn new(all: Vec<DecisionView>, labels: Vec<(String, String)>) -> Self {
        let mut v = Self {
            all: vec![],
            labels: vec![],
            show_closed: false,
            selected: None,
            top: 0,
            view: 1,
            pinned: false,
            detail_top: 0,
        };
        v.set(all, labels);
        v
    }

    /// Live update; the selection stays on its record.
    pub fn set(&mut self, all: Vec<DecisionView>, labels: Vec<(String, String)>) {
        self.all = all;
        self.labels = labels;
        let shown = self.shown();
        if self
            .selected
            .as_ref()
            .is_none_or(|s| !shown.iter().any(|d| d.id == *s))
        {
            self.selected = shown.first().map(|d| d.id.clone());
        }
    }

    fn label(&self, id: &str) -> String {
        self.labels
            .iter()
            .find(|(i, _)| i == id)
            .map_or_else(|| id.to_string(), |(_, l)| l.clone())
    }

    /// Agent ids quoted in a record's text, as display labels.
    fn human(&self, text: &str) -> String {
        let mut out = text.to_string();
        // Longest ids first: a canonical id contains its short form.
        let mut ids: Vec<&(String, String)> = self.labels.iter().collect();
        ids.sort_by_key(|(i, _)| std::cmp::Reverse(i.len()));
        for (id, label) in ids {
            if id.len() > 6 && out.contains(id.as_str()) {
                out = out.replace(id.as_str(), label);
            }
        }
        out
    }

    fn shown(&self) -> Vec<&DecisionView> {
        self.all
            .iter()
            .filter(|d| self.show_closed || d.open())
            .collect()
    }

    fn sel_index(&self) -> Option<usize> {
        let s = self.selected.as_ref()?;
        self.shown().iter().position(|d| d.id == *s)
    }

    fn step(&mut self, d: isize) {
        let shown = self.shown();
        if shown.is_empty() {
            return;
        }
        let i = self.sel_index().unwrap_or(0) as isize;
        let j = (i + d).clamp(0, shown.len() as isize - 1) as usize;
        self.selected = Some(shown[j].id.clone());
        self.pinned = false;
        self.detail_top = 0;
    }

    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.top as isize + delta).max(0) as usize);
    }
    pub fn set_offset(&mut self, top: usize) {
        self.top = top.min(self.shown().len().saturating_sub(self.view));
        self.pinned = true;
    }
    pub fn set_detail_offset(&mut self, top: usize) {
        self.detail_top = top;
    }

    fn act(&mut self, act: u8) -> DecisionsAction {
        if act == ACT_CLOSED {
            self.show_closed = !self.show_closed;
            let shown = self.shown();
            if self
                .selected
                .as_ref()
                .is_none_or(|s| !shown.iter().any(|d| d.id == *s))
            {
                self.selected = shown.first().map(|d| d.id.clone());
            }
            return DecisionsAction::None;
        }
        let Some(d) = self.sel_index().map(|i| self.shown()[i].clone()) else {
            return DecisionsAction::None;
        };
        if !d.open() {
            return DecisionsAction::None;
        }
        match act {
            ACT_KEEP => DecisionsAction::Keep(d.id),
            ACT_CHANGE => DecisionsAction::Change(d.id),
            ACT_DISCUSS => DecisionsAction::Discuss(d.id),
            _ => DecisionsAction::None,
        }
    }

    pub fn click_act(&mut self, act: u8) -> DecisionsAction {
        self.act(act)
    }

    pub fn click(&mut self, i: usize) {
        if let Some(d) = self.shown().get(i) {
            self.selected = Some(d.id.clone());
            self.detail_top = 0;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DecisionsAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return DecisionsAction::Close,
            KeyCode::Char('c') if ctrl => return DecisionsAction::Close,
            KeyCode::Up => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::PageUp => self.step(-(self.view as isize)),
            KeyCode::PageDown => self.step(self.view as isize),
            KeyCode::Tab => return self.act(ACT_CLOSED),
            KeyCode::Char('K') | KeyCode::Char('k') => return self.act(ACT_KEEP),
            KeyCode::Char('c') => return self.act(ACT_CHANGE),
            KeyCode::Char('d') => return self.act(ACT_DISCUSS),
            _ => {}
        }
        DecisionsAction::None
    }

    fn detail(&self, d: &DecisionView, w: usize) -> Vec<Line<'static>> {
        let t = theme();
        let mut out = vec![];
        let field = |out: &mut Vec<Line<'static>>, k: &str, v: String, style: Style| {
            if v.trim().is_empty() {
                return;
            }
            for (i, l) in v.lines().enumerate() {
                out.extend(wrap_lines(
                    vec![Line::from(vec![
                        Span::styled(format!("{:<13}", if i == 0 { k } else { "" }), t.faint()),
                        Span::styled(l.to_string(), style),
                    ])],
                    w,
                    &[],
                    &[Span::raw(" ".repeat(13))],
                ));
            }
        };
        field(&mut out, "question", self.human(&d.question), t.bold());
        field(
            &mut out,
            "choice",
            d.choice.clone(),
            Style::default().fg(t.accent),
        );
        if !d.alternatives.is_empty() {
            field(
                &mut out,
                "alternatives",
                d.alternatives
                    .iter()
                    .map(|a| format!("· {a}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                t.text(),
            );
        }
        field(&mut out, "reason", self.human(&d.reason), t.text());
        field(
            &mut out,
            "decided by",
            format!(
                "{} · {} · {} ago",
                self.label(&d.agent),
                kind_label(&d.kind),
                crate::engine::age(&d.created)
            ),
            t.muted(),
        );
        if !d.files.is_empty() {
            field(
                &mut out,
                "files",
                d.files
                    .iter()
                    .map(|p| crate::transcript::rel_path(p))
                    .collect::<Vec<_>>()
                    .join(", "),
                Style::default().fg(t.link),
            );
        }
        if !d.criteria.is_empty() {
            field(&mut out, "criteria", d.criteria.join(", "), t.muted());
        }
        let (mark, color) = status_mark(&d.status);
        field(
            &mut out,
            "status",
            match &d.review {
                Some(r) => format!("{mark} {} · {r}", d.status),
                None => format!("{mark} {}", d.status),
            },
            Style::default().fg(color),
        );
        out
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let width = screen.width.saturating_sub(4).min(120);
        // An empty ledger gets a small box; records get the screen.
        let height = if self.all.is_empty() {
            screen.height.saturating_sub(2).min(16)
        } else {
            screen.height.saturating_sub(2)
        };
        let area = Rect {
            x: screen.x + (screen.width.saturating_sub(width)) / 2,
            y: screen.y + (screen.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        let open = self.all.iter().filter(|d| d.open()).count();
        let closed = self.all.len() - open;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .title(Span::styled(
                format!(" Decisions made on your behalf · {open} open · {closed} closed "),
                t.bold(),
            ))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(t.surface));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let w = inner.width.saturating_sub(1) as usize;
        let mut y = inner.y;
        // The closed toggle.
        let toggle = format!(
            " tab {} closed ({closed}) ",
            if self.show_closed { "hide" } else { "show" }
        );
        let tw = toggle.width() as u16;
        hits.add(
            Rect {
                x: inner.x,
                y,
                width: tw.min(inner.width),
                height: 1,
            },
            Target::Act(ACT_CLOSED),
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                toggle,
                Style::default().fg(if self.show_closed { t.accent } else { t.fog }),
            ))),
            Rect {
                y,
                height: 1,
                ..inner
            },
        );
        y += 2;
        let shown: Vec<DecisionView> = self.shown().into_iter().cloned().collect();
        let sel = self.sel_index();
        let detail = sel.map(|i| self.detail(&shown[i], w)).unwrap_or_default();
        let footer_h = 2u16;
        let avail = inner.bottom().saturating_sub(y + footer_h + 2);
        if shown.is_empty() {
            let msg = if self.all.is_empty() {
                "No decisions recorded yet.\n\nWhen an agent makes a choice you could have made — answering a scope request, resolving a criteria proposal or roadblock, an auto-mode safety verdict, a fallback model switch, stopping or finalizing an agent, or an explicit decide call for an approach — it is recorded here, with its reason, so you can keep it, change it or discuss it."
            } else {
                "No open decisions. Tab shows the closed ones."
            };
            let lines: Vec<Line<'static>> = msg
                .lines()
                .flat_map(|l| {
                    wrap_lines(
                        vec![Line::from(Span::styled(l.to_string(), t.muted()))],
                        w,
                        &[],
                        &[],
                    )
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines),
                Rect {
                    y,
                    height: avail,
                    ..inner
                },
            );
        } else {
            let detail_h = (detail.len() as u16).min(avail.saturating_sub(4)).min(14);
            let list_h = avail.saturating_sub(detail_h + 1).max(1);
            self.view = list_h as usize;
            if !self.pinned
                && let Some(s) = sel
            {
                if s < self.top {
                    self.top = s;
                } else if s >= self.top + self.view {
                    self.top = s + 1 - self.view;
                }
            }
            self.top = self.top.min(shown.len().saturating_sub(self.view));
            let list = Rect {
                y,
                height: list_h,
                ..inner
            };
            let mut lines = vec![];
            for (i, d) in shown.iter().enumerate().skip(self.top).take(self.view) {
                let on = Some(i) == sel;
                let base = if on {
                    Style::default().bg(t.select_bg)
                } else {
                    Style::default()
                };
                let (mark, color) = status_mark(&d.status);
                let right = format!(
                    "{} · {} · {}",
                    self.label(&d.agent),
                    kind_label(&d.kind),
                    crate::engine::age(&d.created)
                );
                let lead = format!("{} {mark} ", if on { "›" } else { " " });
                let room = w.saturating_sub(lead.width() + right.width() + 2);
                let q = truncate(
                    &format!(
                        "{} → {}",
                        self.human(&d.question).replace('\n', " "),
                        d.choice.replace('\n', " ")
                    ),
                    room,
                );
                let pad = room.saturating_sub(q.width());
                lines.push(Line::from(vec![
                    Span::styled(lead, base.fg(color)),
                    Span::styled(
                        q,
                        if on {
                            base.fg(t.hover_fg).add_modifier(Modifier::BOLD)
                        } else if d.open() {
                            base.fg(t.ink)
                        } else {
                            base.fg(t.dim)
                        },
                    ),
                    Span::styled(" ".repeat(pad + 2), base),
                    Span::styled(right, base.fg(t.dim)),
                ]));
                hits.add(
                    Rect {
                        y: list.y + (i - self.top) as u16,
                        height: 1,
                        width: list.width.saturating_sub(1),
                        ..list
                    },
                    Target::Row(i),
                );
            }
            frame.render_widget(Paragraph::new(lines), list);
            ui::scrollbar(
                frame.buffer_mut(),
                hits,
                ScrollId::Overlay,
                list,
                shown.len(),
                self.view,
                self.top,
            );
            y += list_h;
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(inner.width as usize),
                    Style::default().fg(t.rule),
                ))),
                Rect {
                    y,
                    height: 1,
                    ..inner
                },
            );
            y += 1;
            let detail_area = Rect {
                y,
                height: detail_h,
                ..inner
            };
            self.detail_top = self
                .detail_top
                .min(detail.len().saturating_sub(detail_h as usize));
            frame.render_widget(
                Paragraph::new(
                    detail
                        .iter()
                        .skip(self.detail_top)
                        .take(detail_h as usize)
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                detail_area,
            );
            hits.region(detail_area, ScrollId::OverlayList);
            ui::scrollbar(
                frame.buffer_mut(),
                hits,
                ScrollId::OverlayList,
                detail_area,
                detail.len(),
                detail_h as usize,
                self.detail_top,
            );
        }
        // Actions on the selected open record.
        let can = sel.is_some_and(|i| shown[i].open());
        let ay = inner.bottom().saturating_sub(footer_h + 1);
        let mut x = inner.x;
        let mut spans = vec![];
        for (act, label) in [
            (ACT_KEEP, "k keep"),
            (ACT_CHANGE, "c change…"),
            (ACT_DISCUSS, "d discuss…"),
        ] {
            let text = format!(" {label} ");
            let cw = text.width() as u16;
            if can {
                hits.add(
                    Rect {
                        x,
                        y: ay,
                        width: cw,
                        height: 1,
                    },
                    Target::Act(act),
                );
            }
            spans.push(Span::styled(
                text,
                if can {
                    Style::default().fg(t.link)
                } else {
                    t.faint()
                },
            ));
            spans.push(Span::raw(" "));
            x += cw + 1;
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                y: ay,
                height: 1,
                ..inner
            },
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(
                    "↑↓ select · k keep (close) · c change: steer the deciding agent · d discuss privately · tab closed · esc close",
                    w,
                ),
                t.faint(),
            ))),
            Rect {
                y: inner.bottom().saturating_sub(1),
                height: 1,
                ..inner
            },
        );
    }
}
