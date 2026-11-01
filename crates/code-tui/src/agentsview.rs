//! `/agents` (D131): the whole session tree as a filterable tree view with
//! live rows, and actions on a selected descendant (open its transcript,
//! ask its status through a hidden fork, finalize, stop). The filters are
//! the `agents list` filters: state, context, subtree, model and a text
//! query over titles, briefs and reports.
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

/// One agent row, in display terms.
#[derive(Clone, Debug, Default)]
pub struct AgentRow {
    /// Canonical id (root is `root`).
    pub id: String,
    pub label: String,
    pub title: String,
    pub parent: Option<String>,
    /// active, waiting, done, failed or stopped.
    pub state: String,
    pub model: String,
    pub elapsed: u64,
    pub tokens: u64,
    pub cost: String,
    pub last: String,
    pub descendants: u64,
    pub scope: String,
    /// current or earlier (relative to root's context).
    pub context: String,
    pub last_text: String,
    pub finalizing: bool,
    /// Display depth within the shown rows.
    pub depth: usize,
    /// Start order (siblings oldest first).
    pub order: usize,
}

pub enum AgentsAction {
    None,
    Close,
    /// Filters changed: query the engine again.
    Refresh,
    Open(String),
    Ask(String),
    Finalize(String),
    Stop(String),
}

const STATES: [Option<&str>; 6] = [
    None,
    Some("active"),
    Some("waiting"),
    Some("done"),
    Some("failed"),
    Some("stopped"),
];
const CONTEXTS: [Option<&str>; 3] = [None, Some("current"), Some("earlier")];

/// Action chip ids (`Target::Act`).
const ACT_STATE: u8 = 0;
const ACT_CONTEXT: u8 = 1;
const ACT_SUBTREE: u8 = 2;
const ACT_MODEL: u8 = 3;
const ACT_SEARCH: u8 = 4;
const ACT_OPEN: u8 = 10;
const ACT_ASK: u8 = 11;
const ACT_FINALIZE: u8 = 12;
const ACT_STOP: u8 = 13;

pub struct AgentsView {
    pub rows: Vec<AgentRow>,
    /// Models seen in the unfiltered tree (the model filter cycles them).
    pub models: Vec<String>,
    state: usize,
    context: usize,
    /// Only this agent and its descendants.
    pub subtree: Option<(String, String)>,
    model: Option<usize>,
    pub query: String,
    query_focus: bool,
    /// Flat mode: no finalize (D133).
    pub flat: bool,
    selected: Option<String>,
    top: usize,
    view: usize,
    pinned: bool,
    /// The latest status answer: (agent, question, answer or error).
    pub answer: Option<(String, String, Result<String, String>)>,
    /// An ask in flight: the agent.
    pub asking: Option<String>,
    detail_top: usize,
}

pub fn fmt_elapsed(s: u64) -> String {
    match s {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn tokens(n: u64) -> String {
    crate::picker::fmt_tokens(n)
}

pub fn state_icon(state: &str) -> (&'static str, ratatui::style::Color) {
    let t = theme();
    match state {
        "active" => ("●", t.yellow),
        "waiting" => ("◌", t.yellow),
        "done" => ("✓", t.green),
        "failed" => ("✗", t.red),
        "stopped" => ("■", t.dim),
        _ => ("·", t.dim),
    }
}

impl AgentsView {
    pub fn new(flat: bool) -> Self {
        Self {
            rows: vec![],
            models: vec![],
            state: 0,
            context: 0,
            subtree: None,
            model: None,
            query: String::new(),
            query_focus: false,
            flat,
            selected: None,
            top: 0,
            view: 1,
            pinned: false,
            answer: None,
            asking: None,
            detail_top: 0,
        }
    }

    /// The current filters: (state, context, subtree, model, query).
    pub fn filters(&self) -> (Option<&str>, Option<&str>, Option<&str>, Option<&str>, &str) {
        (
            STATES[self.state],
            CONTEXTS[self.context],
            self.subtree.as_ref().map(|(id, _)| id.as_str()),
            self.model
                .and_then(|i| self.models.get(i))
                .map(String::as_str),
            &self.query,
        )
    }

    /// New rows (already filtered by the engine), arranged as a tree.
    pub fn set_rows(&mut self, mut rows: Vec<AgentRow>) {
        // Parents before children, siblings oldest first; a row whose
        // parent is filtered out hangs at the top level.
        let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        let mut out: Vec<AgentRow> = vec![];
        fn walk(
            parent: Option<&str>,
            depth: usize,
            rows: &mut Vec<AgentRow>,
            ids: &[String],
            out: &mut Vec<AgentRow>,
        ) {
            let mut i = 0;
            while i < rows.len() {
                let p = rows[i].parent.as_deref();
                let top_level = parent.is_none() && p.is_none_or(|p| !ids.iter().any(|x| x == p));
                if top_level || (parent.is_some() && p == parent) {
                    let mut r = rows.remove(i);
                    r.depth = depth;
                    let id = r.id.clone();
                    out.push(r);
                    walk(Some(&id), depth + 1, rows, ids, out);
                } else {
                    i += 1;
                }
            }
        }
        rows.sort_by_key(|r| r.order);
        walk(None, 0, &mut rows, &ids, &mut out);
        out.append(&mut rows);
        self.rows = out;
        if self
            .selected
            .as_ref()
            .is_none_or(|s| !self.rows.iter().any(|r| r.id == *s))
        {
            self.selected = self
                .rows
                .iter()
                .find(|r| r.id != "root")
                .or(self.rows.first())
                .map(|r| r.id.clone());
        }
    }

    pub fn select(&mut self, id: &str) {
        if self.rows.iter().any(|r| r.id == id) {
            self.selected = Some(id.to_string());
            self.pinned = false;
        }
    }

    fn sel_index(&self) -> Option<usize> {
        let s = self.selected.as_ref()?;
        self.rows.iter().position(|r| r.id == *s)
    }

    pub fn selected(&self) -> Option<&AgentRow> {
        self.sel_index().map(|i| &self.rows[i])
    }

    fn step(&mut self, d: isize) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let i = self.sel_index().unwrap_or(0) as isize;
        let j = (i + d).clamp(0, n as isize - 1) as usize;
        self.selected = Some(self.rows[j].id.clone());
        self.pinned = false;
        self.detail_top = 0;
    }

    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.top as isize + delta).max(0) as usize);
    }
    pub fn set_offset(&mut self, top: usize) {
        self.top = top.min(self.rows.len().saturating_sub(self.view));
        self.pinned = true;
    }

    fn cycle(&mut self, act: u8, back: bool) -> AgentsAction {
        let step = |i: usize, n: usize| if back { (i + n - 1) % n } else { (i + 1) % n };
        match act {
            ACT_STATE => self.state = step(self.state, STATES.len()),
            ACT_CONTEXT => self.context = step(self.context, CONTEXTS.len()),
            ACT_SUBTREE => {
                self.subtree = match (&self.subtree, self.selected()) {
                    (Some(_), _) => None,
                    (None, Some(r)) if r.id != "root" => Some((r.id.clone(), r.label.clone())),
                    (None, _) => None,
                }
            }
            ACT_MODEL => {
                let n = self.models.len() + 1;
                let cur = self.model.map_or(0, |i| i + 1);
                let next = step(cur, n);
                self.model = next.checked_sub(1);
            }
            ACT_SEARCH => {
                self.query_focus = true;
                return AgentsAction::None;
            }
            _ => return AgentsAction::None,
        }
        AgentsAction::Refresh
    }

    fn act(&self, act: u8) -> AgentsAction {
        let Some(r) = self.selected() else {
            return AgentsAction::None;
        };
        let id = r.id.clone();
        match act {
            ACT_OPEN => AgentsAction::Open(id),
            ACT_ASK => AgentsAction::Ask(id),
            ACT_FINALIZE => AgentsAction::Finalize(id),
            ACT_STOP => AgentsAction::Stop(id),
            _ => AgentsAction::None,
        }
    }

    /// A click on a filter or action chip.
    pub fn click_act(&mut self, act: u8) -> AgentsAction {
        if act < ACT_OPEN {
            self.cycle(act, false)
        } else {
            self.act(act)
        }
    }

    /// A click on row `i`: select it (a second click opens it).
    pub fn click(&mut self, i: usize) -> AgentsAction {
        let Some(r) = self.rows.get(i) else {
            return AgentsAction::None;
        };
        if self.selected.as_deref() == Some(r.id.as_str()) {
            return AgentsAction::Open(r.id.clone());
        }
        self.selected = Some(r.id.clone());
        self.detail_top = 0;
        AgentsAction::None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AgentsAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.query_focus {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Down | KeyCode::Tab => {
                    self.query_focus = false;
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    return AgentsAction::Refresh;
                }
                KeyCode::Char('u') if ctrl => {
                    self.query.clear();
                    return AgentsAction::Refresh;
                }
                KeyCode::Char(c) if !ctrl => {
                    self.query.push(c);
                    return AgentsAction::Refresh;
                }
                _ => {}
            }
            return AgentsAction::None;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                return AgentsAction::Refresh;
            }
            KeyCode::Esc | KeyCode::Char('q') => return AgentsAction::Close,
            KeyCode::Char('c') if ctrl => return AgentsAction::Close,
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::PageUp => self.step(-(self.view as isize)),
            KeyCode::PageDown => self.step(self.view as isize),
            KeyCode::Home => self.step(-(self.rows.len() as isize)),
            KeyCode::End => self.step(self.rows.len() as isize),
            KeyCode::Char('/') => self.query_focus = true,
            KeyCode::Char('1') => return self.cycle(ACT_STATE, shift),
            KeyCode::Char('2') => return self.cycle(ACT_CONTEXT, shift),
            KeyCode::Char('3') => return self.cycle(ACT_SUBTREE, shift),
            KeyCode::Char('4') => return self.cycle(ACT_MODEL, shift),
            KeyCode::Enter | KeyCode::Char('o') => return self.act(ACT_OPEN),
            KeyCode::Char('a') => return self.act(ACT_ASK),
            KeyCode::Char('f') if !self.flat => return self.act(ACT_FINALIZE),
            KeyCode::Char('s') | KeyCode::Char('x') => return self.act(ACT_STOP),
            _ => {}
        }
        AgentsAction::None
    }

    fn row_line(&self, r: &AgentRow, sel: bool, w: usize) -> Line<'static> {
        let t = theme();
        let (icon, color) = state_icon(&r.state);
        let base = if sel {
            Style::default().bg(t.select_bg)
        } else {
            Style::default()
        };
        let indent = if r.depth == 0 {
            String::new()
        } else {
            format!("{}└ ", "  ".repeat(r.depth - 1))
        };
        let lead = format!("{}{indent}", if sel { "›" } else { " " });
        let mut right = format!(
            "{} {}",
            if r.finalizing && matches!(r.state.as_str(), "active" | "waiting") {
                "finalizing".to_string()
            } else {
                r.state.clone()
            },
            fmt_elapsed(r.elapsed)
        );
        if w >= 70 {
            right.push_str(&format!(" · {} tok · {}", tokens(r.tokens), r.cost));
        }
        if r.descendants > 0 {
            right.push_str(&format!(" · ↳{}", r.descendants));
        }
        let label = format!("{icon} {} ", r.label);
        let room = w.saturating_sub(lead.width() + label.width() + right.width() + 2);
        let title = truncate(&r.title.replace('\n', " "), room);
        let pad = room.saturating_sub(title.width());
        Line::from(vec![
            Span::styled(lead, base.fg(t.accent)),
            Span::styled(format!("{icon} "), base.fg(color)),
            Span::styled(
                format!("{} ", r.label),
                base.fg(if sel { t.hover_fg } else { t.ink })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(title, base.fg(t.fog)),
            Span::styled(" ".repeat(pad + 2), base),
            Span::styled(right, base.fg(t.dim)),
        ])
    }

    fn detail_lines(&self, w: usize) -> Vec<Line<'static>> {
        let t = theme();
        let Some(r) = self.selected() else {
            return vec![];
        };
        let mut out = vec![];
        let (icon, color) = state_icon(&r.state);
        let mut head = vec![
            Span::styled(format!("{icon} "), Style::default().fg(color)),
            Span::styled(r.label.clone(), t.bold()),
        ];
        if !r.title.is_empty() {
            head.push(Span::styled(format!("  {}", r.title), t.text()));
        }
        out.extend(wrap_lines(
            vec![Line::from(head)],
            w,
            &[],
            &[Span::raw("  ")],
        ));
        let facts = [
            ("model", r.model.clone()),
            (
                "state",
                format!(
                    "{}{} · {} · {} tokens · {}",
                    r.state,
                    if r.finalizing { " (finalizing)" } else { "" },
                    fmt_elapsed(r.elapsed),
                    tokens(r.tokens),
                    r.cost
                ),
            ),
            ("last", r.last.clone()),
            (
                "tree",
                format!(
                    "{} descendant{} · {} context",
                    r.descendants,
                    if r.descendants == 1 { "" } else { "s" },
                    r.context
                ),
            ),
            ("scope", r.scope.clone()),
        ];
        for (k, v) in facts {
            if v.trim().is_empty() {
                continue;
            }
            out.extend(wrap_lines(
                vec![Line::from(vec![
                    Span::styled(format!("{k:<6}"), t.faint()),
                    Span::styled(v, t.text()),
                ])],
                w,
                &[],
                &[Span::raw("      ")],
            ));
        }
        match (&self.answer, &self.asking) {
            (_, Some(a)) if *a == r.id => {
                out.push(Line::from(Span::styled(
                    "status  asking a read-only fork of its context…",
                    Style::default().fg(t.yellow),
                )));
            }
            (Some((a, q, res)), _) if *a == r.id => {
                out.extend(wrap_lines(
                    vec![Line::from(vec![
                        Span::styled("asked ", t.faint()),
                        Span::styled(q.clone(), t.muted()),
                    ])],
                    w,
                    &[],
                    &[Span::raw("      ")],
                ));
                let (style, text) = match res {
                    Ok(a) => (t.text(), a.clone()),
                    Err(e) => (Style::default().fg(t.red), format!("failed: {e}")),
                };
                for l in text.lines() {
                    out.extend(wrap_lines(
                        vec![Line::from(Span::styled(l.to_string(), style))],
                        w,
                        &[Span::raw("      ")],
                        &[Span::raw("      ")],
                    ));
                }
            }
            _ if !r.last_text.trim().is_empty() => {
                let text = r.last_text.split_whitespace().collect::<Vec<_>>().join(" ");
                out.extend(wrap_lines(
                    vec![Line::from(vec![
                        Span::styled("said  ", t.faint()),
                        Span::styled(text, t.muted()),
                    ])],
                    w,
                    &[],
                    &[Span::raw("      ")],
                ));
            }
            _ => {}
        }
        out
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let width = screen.width.saturating_sub(4).min(130);
        let height = screen.height.saturating_sub(2);
        let area = Rect {
            x: screen.x + (screen.width.saturating_sub(width)) / 2,
            y: screen.y + 1,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        let n_agents = self.rows.iter().filter(|r| r.id != "root").count();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .title(Span::styled(
                format!(
                    " Agents · {n_agents} shown{} ",
                    if self.flat { " · flat" } else { " · tree" }
                ),
                t.bold(),
            ))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(t.surface));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let w = inner.width.saturating_sub(1) as usize;
        let mut y = inner.y;
        // Filter chips: click (or 1–4) cycles each.
        let (state, context, _, model, _) = self.filters();
        let (state, context, model) = (
            state.map(str::to_string),
            context.map(str::to_string),
            model.map(str::to_string),
        );
        let chips: Vec<(u8, String, bool)> = vec![
            (
                ACT_STATE,
                format!("1 state: {}", state.as_deref().unwrap_or("any")),
                state.is_some(),
            ),
            (
                ACT_CONTEXT,
                format!("2 context: {}", context.as_deref().unwrap_or("any")),
                context.is_some(),
            ),
            (
                ACT_SUBTREE,
                format!(
                    "3 subtree: {}",
                    self.subtree.as_ref().map_or("all", |(_, l)| l.as_str())
                ),
                self.subtree.is_some(),
            ),
            (
                ACT_MODEL,
                format!(
                    "4 model: {}",
                    model
                        .as_deref()
                        .map_or("any", |m| m.rsplit('/').next().unwrap_or(m))
                ),
                model.is_some(),
            ),
        ];
        let mut x = inner.x;
        let mut spans = vec![];
        for (act, label, on) in chips {
            let text = format!(" {label} ");
            let cw = text.width() as u16;
            if x + cw > inner.right() {
                break;
            }
            hits.add(
                Rect {
                    x,
                    y,
                    width: cw,
                    height: 1,
                },
                Target::Act(act),
            );
            spans.push(Span::styled(
                text,
                if on {
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.fog)
                },
            ));
            spans.push(Span::raw(" "));
            x += cw + 1;
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                y,
                height: 1,
                ..inner
            },
        );
        y += 1;
        // The query field.
        let q = if self.query.is_empty() && !self.query_focus {
            Span::styled("/ to search titles, briefs and reports", t.faint())
        } else {
            Span::styled(
                format!("{}{}", self.query, if self.query_focus { "▏" } else { "" }),
                t.text(),
            )
        };
        let search = Rect {
            y,
            height: 1,
            ..inner
        };
        hits.add(search, Target::Act(ACT_SEARCH));
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "search › ",
                    Style::default().fg(if self.query_focus { t.accent } else { t.dim }),
                ),
                q,
            ])),
            search,
        );
        y += 2;
        // Detail pane and actions at the bottom.
        let detail = self.detail_lines(w);
        let footer_h = 2u16;
        let actions_h = 1u16;
        let avail = inner.bottom().saturating_sub(y + footer_h + actions_h + 1);
        let detail_h = (detail.len() as u16)
            .min(avail.saturating_sub(3).max(1))
            .min((avail / 2).max(6));
        let list_h = avail.saturating_sub(detail_h + 1).max(1);
        self.view = list_h as usize;
        let list = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: list_h,
        };
        let sel = self.sel_index();
        if !self.pinned
            && let Some(s) = sel
        {
            if s < self.top {
                self.top = s;
            } else if s >= self.top + self.view {
                self.top = s + 1 - self.view;
            }
        }
        self.top = self.top.min(self.rows.len().saturating_sub(self.view));
        let mut lines = vec![];
        if self.rows.is_empty() {
            lines.extend(wrap_lines(vec![Line::from(Span::styled(
                if self.query.is_empty() && state.is_none() && context.is_none() && model.is_none() && self.subtree.is_none() {
                    "no subagents yet · agents appear here when the model starts `task` children"
                } else {
                    "nothing matches the filters · esc clears the search, click a filter to change it"
                },
                t.faint(),
            ))], w, &[], &[]));
        }
        let only_root = self.rows.len() == 1 && self.rows[0].id == "root";
        for (i, r) in self.rows.iter().enumerate().skip(self.top).take(self.view) {
            lines.push(self.row_line(r, Some(i) == sel, w));
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
        if only_root {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  no subagents yet · they appear here, live, when the model starts task children",
                t.faint(),
            )));
        }
        frame.render_widget(Paragraph::new(lines), list);
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::Overlay,
            list,
            self.rows.len(),
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
        y += detail_h;
        // Action chips for the selected agent.
        let selected = self.selected().cloned();
        let root = selected.as_ref().is_none_or(|r| r.id == "root");
        let live = selected
            .as_ref()
            .is_some_and(|r| matches!(r.state.as_str(), "active" | "waiting"));
        let mut acts = vec![(ACT_OPEN, "⏎ open transcript", true)];
        acts.push((ACT_ASK, "a ask status", !root));
        if !self.flat {
            acts.push((ACT_FINALIZE, "f finalize", !root && live));
        }
        acts.push((ACT_STOP, "s stop branch", !root && live));
        let mut x = inner.x;
        let mut spans = vec![];
        for (act, label, enabled) in acts {
            let text = format!(" {label} ");
            let cw = text.width() as u16;
            if x + cw > inner.right() {
                break;
            }
            if enabled {
                hits.add(
                    Rect {
                        x,
                        y,
                        width: cw,
                        height: 1,
                    },
                    Target::Act(act),
                );
            }
            spans.push(Span::styled(
                text,
                if enabled {
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
                y,
                height: 1,
                ..inner
            },
        );
        let keys = if self.query_focus {
            "type to search · enter/esc done · ctrl+u clear"
        } else {
            "↑↓ select · click twice or ⏎ open · 1-4 filters (shift: back) · / search · esc close"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(keys, w), t.faint()))),
            Rect {
                y: inner.bottom().saturating_sub(1),
                height: 1,
                ..inner
            },
        );
    }

    /// The detail pane's scrollbar or wheel.
    pub fn set_detail_offset(&mut self, top: usize) {
        self.detail_top = top;
    }
}
