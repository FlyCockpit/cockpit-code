//! `/model` picker: models grouped by provider with label, context,
//! capabilities and benchmarks. Typing filters (fuzzy, case-insensitive);
//! Enter activates, Space (empty query) or Ctrl+O toggles subagent
//! availability, Ctrl+P toggles a provider, Ctrl+B cycles the sort
//! benchmark, Ctrl+S marks the safety model, Ctrl+R re-runs model
//! discovery. Toggles are session-only and live in the engine adapter.
//! Rows, headers and the toggle/badge columns are clickable. A provider
//! whose discovery failed carries a warning badge on its header.
use crate::composer::TextBuf;
use crate::engine::{Catalog, ModelInfo};
use crate::theme::theme;
use crate::ui::{self, Hits, ScrollId, Target};
use crate::wrap::{truncate, wrap_lines};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use std::collections::BTreeSet;
use unicode_width::UnicodeWidthStr;

pub enum Action {
    None,
    Close,
    Activate(String),
    ToggleModel(String),
    ToggleProvider(String),
    /// Mark this model as the session's safety model.
    SetSafety(String),
    /// Re-run discovery for every discovering provider.
    Refresh,
}

/// Picker cell columns (for `Target::Cell`).
pub const CELL_SUB: u8 = 0;
pub const CELL_SAFETY: u8 = 1;

enum Row {
    Header(String),
    Model(usize),
    /// A provider without models (discovery pending or failed).
    Empty(String),
}

pub struct Picker {
    catalog: Catalog,
    active: Option<String>,
    pub disabled_models: BTreeSet<String>,
    pub disabled_providers: BTreeSet<String>,
    filter: TextBuf,
    /// The session's safety model (shield badge).
    pub safety: Option<String>,
    /// Opened from `/permissions`: Enter chooses the safety model.
    pub choose_safety: bool,
    /// A title replacing "Models" when Enter picks for another purpose
    /// (`/exclusive`).
    pub heading: Option<String>,
    view: usize,
    pinned: bool,
    /// Index into `catalog.benchmarks`; None sorts by label.
    sort: Option<usize>,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    pub message: Option<String>,
    /// Providers whose discovery is running.
    pub discovering: BTreeSet<String>,
}

pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1_000_000.0;
        if m.fract() == 0.0 {
            format!("{m:.0}M")
        } else {
            format!("{m:.1}M")
        }
    } else if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

fn fmt_bench(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else if v.abs() < 1.0 {
        format!("{v:.3}")
    } else {
        format!("{v:.1}")
    }
}

impl Picker {
    pub fn new(
        catalog: Catalog,
        active: Option<String>,
        disabled_models: BTreeSet<String>,
        disabled_providers: BTreeSet<String>,
    ) -> Self {
        let mut p = Self {
            catalog,
            active,
            disabled_models,
            disabled_providers,
            filter: TextBuf::default(),
            safety: None,
            choose_safety: false,
            heading: None,
            view: 1,
            pinned: false,
            sort: None,
            rows: vec![],
            selected: 0,
            scroll: 0,
            message: None,
            discovering: BTreeSet::new(),
        };
        p.rebuild();
        if let Some(a) = &p.active
            && let Some(i) = p
                .rows
                .iter()
                .position(|r| matches!(r, Row::Model(m) if &p.catalog.models[*m].id == a))
        {
            p.selected = i;
        }
        p
    }

    /// Swap in a refreshed catalog (discovery finished), keeping the
    /// selection, filter and toggles.
    pub fn set_catalog(&mut self, catalog: Catalog) {
        let keep = self.current().map(|m| m.id.clone());
        self.catalog = catalog;
        self.sort = self.sort.filter(|&s| s < self.catalog.benchmarks.len());
        self.rebuild();
        if let Some(id) = keep
            && let Some(i) = self
                .rows
                .iter()
                .position(|r| matches!(r, Row::Model(m) if self.catalog.models[*m].id == id))
        {
            self.selected = i;
        }
    }

    fn rebuild(&mut self) {
        let keep = self.current().map(|m| m.id.clone());
        let needle = self.filter.text.to_lowercase();
        let mut idx: Vec<usize> = (0..self.catalog.models.len())
            .filter(|&i| {
                needle.is_empty() || {
                    let m = &self.catalog.models[i];
                    fuzzy(m, &haystack(m), &needle)
                }
            })
            .collect();
        let sort_key = self.sort.map(|s| self.catalog.benchmarks[s].key.clone());
        let higher = sort_key
            .as_deref()
            .is_none_or(|k| self.catalog.higher_is_better(k));
        let cmp_values = |va: Option<f64>, vb: Option<f64>| match (va, vb) {
            (Some(x), Some(y)) => {
                let o = x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal);
                if higher { o.reverse() } else { o }
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        // With a sort benchmark, providers are ordered by their best model.
        let best = |provider: &str| -> Option<f64> {
            let k = sort_key.as_deref()?;
            self.catalog
                .models
                .iter()
                .filter(|m| m.provider == provider)
                .filter_map(|m| bench(m, k))
                .reduce(|a, b| if (a > b) == higher { a } else { b })
        };
        idx.sort_by(|&a, &b| {
            let (ma, mb) = (&self.catalog.models[a], &self.catalog.models[b]);
            let by_provider = if ma.provider == mb.provider {
                std::cmp::Ordering::Equal
            } else {
                cmp_values(best(&ma.provider), best(&mb.provider))
                    .then_with(|| ma.provider.cmp(&mb.provider))
            };
            by_provider.then_with(|| match &sort_key {
                None => ma.label.to_lowercase().cmp(&mb.label.to_lowercase()),
                Some(k) => {
                    cmp_values(bench(ma, k), bench(mb, k)).then_with(|| ma.label.cmp(&mb.label))
                }
            })
        });
        self.rows.clear();
        let mut last = None;
        for i in idx {
            let p = &self.catalog.models[i].provider;
            if last.as_ref() != Some(p) {
                self.rows.push(Row::Header(p.clone()));
                last = Some(p.clone());
            }
            self.rows.push(Row::Model(i));
        }
        // Discovering providers with no models yet still show, with their
        // state, so a failed or pending discovery is visible.
        if needle.is_empty() {
            for p in &self.catalog.providers {
                if p.discovers && !self.catalog.models.iter().any(|m| m.provider == p.id) {
                    self.rows.push(Row::Header(p.id.clone()));
                    self.rows.push(Row::Empty(p.id.clone()));
                }
            }
        }
        self.selected = keep
            .and_then(|id| {
                self.rows
                    .iter()
                    .position(|r| matches!(r, Row::Model(m) if self.catalog.models[*m].id == id))
            })
            .or_else(|| self.rows.iter().position(|r| matches!(r, Row::Model(_))))
            .unwrap_or(0);
    }

    fn current(&self) -> Option<&ModelInfo> {
        match self.rows.get(self.selected) {
            Some(Row::Model(i)) => Some(&self.catalog.models[*i]),
            _ => None,
        }
    }

    fn current_provider(&self) -> Option<String> {
        match self.rows.get(self.selected) {
            Some(Row::Model(i)) => Some(self.catalog.models[*i].provider.clone()),
            Some(Row::Header(p)) | Some(Row::Empty(p)) => Some(p.clone()),
            None => None,
        }
    }

    fn step(&mut self, down: bool) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let mut i = self.selected;
        for _ in 0..n {
            i = if down { (i + 1) % n } else { (i + n - 1) % n };
            if matches!(self.rows[i], Row::Model(_)) {
                self.selected = i;
                return;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        self.message = None;
        self.pinned = false;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if !self.filter.text.is_empty() => {
                self.filter.clear();
                self.rebuild();
            }
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('c') if ctrl => return Action::Close,
            KeyCode::Up => self.step(false),
            KeyCode::Down => self.step(true),
            KeyCode::PageUp => (0..10).for_each(|_| self.step(false)),
            KeyCode::PageDown => (0..10).for_each(|_| self.step(true)),
            KeyCode::Backspace => {
                self.filter.backspace();
                self.rebuild();
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.rebuild();
            }
            KeyCode::Char('w') if ctrl => {
                self.filter.delete_word();
                self.rebuild();
            }
            KeyCode::Char('b') if ctrl => self.cycle_sort(),
            KeyCode::Char('p') if ctrl => return self.toggle_provider(),
            KeyCode::Char('o') if ctrl => return self.toggle_model(),
            KeyCode::Char('s') if ctrl => return self.mark_safety(),
            KeyCode::Char('r') if ctrl => return Action::Refresh,
            KeyCode::Char(' ') if self.filter.text.is_empty() => return self.toggle_model(),
            // `/` was the old filter key; it is a no-op on an empty query.
            KeyCode::Char('/') if self.filter.text.is_empty() => {}
            KeyCode::Char(c) if !ctrl => {
                self.filter.insert(c.encode_utf8(&mut [0; 4]));
                self.rebuild();
            }
            KeyCode::Enter => return self.activate(),
            _ => {}
        }
        Action::None
    }

    fn cycle_sort(&mut self) {
        let n = self.catalog.benchmarks.len();
        self.sort = match self.sort {
            None if n > 0 => Some(0),
            Some(i) if i + 1 < n => Some(i + 1),
            _ => None,
        };
        self.rebuild();
    }

    fn activate(&mut self) -> Action {
        if self.choose_safety {
            return self.mark_safety();
        }
        if let Some(m) = self.current() {
            if !m.chat_capable {
                self.message =
                    Some("media-output models are reachable only through generate".into());
                return Action::None;
            }
            return Action::Activate(m.id.clone());
        }
        Action::None
    }

    fn toggle_model(&mut self) -> Action {
        if let Some(m) = self.current() {
            if !m.subagent_capable {
                self.message = Some("config excludes this model from subagents".into());
                return Action::None;
            }
            let id = m.id.clone();
            if !self.disabled_models.remove(&id) {
                self.disabled_models.insert(id.clone());
            }
            return Action::ToggleModel(id);
        }
        Action::None
    }

    fn toggle_provider(&mut self) -> Action {
        if let Some(p) = self.current_provider() {
            if !self.disabled_providers.remove(&p) {
                self.disabled_providers.insert(p.clone());
            }
            return Action::ToggleProvider(p);
        }
        Action::None
    }

    fn mark_safety(&mut self) -> Action {
        if let Some(m) = self.current() {
            if !m.chat_capable || !m.output.iter().all(|o| o == "text") && !m.output.is_empty() {
                self.message = Some("the safety model must be a text model".into());
                return Action::None;
            }
            let id = m.id.clone();
            self.safety = Some(id.clone());
            return Action::SetSafety(id);
        }
        Action::None
    }

    /// A click on row `i` (model: activate, header: toggle the provider).
    pub fn click(&mut self, i: usize) -> Action {
        self.message = None;
        if i >= self.rows.len() {
            return Action::None;
        }
        self.selected = i;
        match self.rows[i] {
            Row::Header(_) => self.toggle_provider(),
            Row::Model(_) => self.activate(),
            Row::Empty(_) => Action::None,
        }
    }

    /// A click on a column cell of row `i`.
    pub fn click_cell(&mut self, i: usize, cell: u8) -> Action {
        if !matches!(self.rows.get(i), Some(Row::Model(_))) {
            return Action::None;
        }
        self.selected = i;
        match cell {
            CELL_SUB => self.toggle_model(),
            _ => self.mark_safety(),
        }
    }

    /// The wheel scrolls the rows; the selection stays (D130).
    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.scroll as isize + delta).max(0) as usize);
    }

    pub fn set_offset(&mut self, top: usize) {
        self.scroll = top.min(self.rows.len().saturating_sub(self.view));
        self.pinned = true;
    }

    fn sub_state(&self, m: &ModelInfo) -> (&'static str, Style) {
        let t = theme();
        if !m.subagent_capable {
            ("–", t.faint())
        } else if self.disabled_providers.contains(&m.provider)
            || self.disabled_models.contains(&m.id)
        {
            ("✗", Style::default().fg(t.red))
        } else {
            ("✓", Style::default().fg(t.green))
        }
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let width = screen.width.saturating_sub(2).min(120);
        let sort_label = self
            .sort
            .map(|s| {
                let b = &self.catalog.benchmarks[s];
                format!("{} {}", b.label, if b.higher_is_better { "↓" } else { "↑" })
            })
            .unwrap_or_else(|| "name".into());
        let count = self
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Model(_)))
            .count();
        let what = if let Some(h) = &self.heading {
            h.as_str()
        } else if self.choose_safety {
            "Choose safety model"
        } else {
            "Models"
        };
        let title = if self.filter.text.is_empty() {
            format!(" {what} · sort: {sort_label} · type to search ")
        } else {
            format!(
                " {what} · search: {}▏ · {count} of {} · sort: {sort_label} ",
                self.filter.text,
                self.catalog.models.len()
            )
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .title(Span::styled(title, t.bold()))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(t.surface));
        // One padding column is the list scrollbar.
        let w = width.saturating_sub(5) as usize;

        // Detail pane for the selected model.
        let mut detail: Vec<Line<'static>> = vec![];
        if let Some(m) = self.current() {
            detail.push(Line::from(vec![
                Span::styled(m.label.clone(), t.bold()),
                Span::styled(format!("  {}", m.id), t.faint()),
            ]));
            if !m.description.is_empty() {
                detail.extend(wrap_lines(
                    vec![Line::from(Span::styled(m.description.clone(), t.muted()))],
                    w,
                    &[],
                    &[],
                ));
            }
            let mut facts = vec![];
            if let Some(c) = m.context_window {
                facts.push(format!("context {}", fmt_tokens(c)));
            }
            if let Some(o) = m.max_output {
                facts.push(format!("max out {}", fmt_tokens(o)));
            }
            if !m.efforts.is_empty() {
                facts.push(format!(
                    "effort {}{}",
                    m.efforts.join("/"),
                    m.default_effort
                        .as_ref()
                        .map(|d| format!(" (default {d})"))
                        .unwrap_or_default()
                ));
            }
            if let Some(c) = &m.cost {
                let r = |v: Option<f64>| v.map(|v| format!("${v}")).unwrap_or("?".into());
                facts.push(format!(
                    "$/MTok in {} out {} cache r {} w {}",
                    r(c.input),
                    r(c.output),
                    r(c.cache_read),
                    r(c.cache_write)
                ));
            }
            detail.extend(wrap_lines(
                vec![Line::from(Span::styled(facts.join(" · "), t.muted()))],
                w,
                &[],
                &[],
            ));
            let caps = format!(
                "in: {} · out: {}{}",
                if m.input.is_empty() {
                    "text".into()
                } else {
                    m.input.join(",")
                },
                if m.output.is_empty() {
                    "text".into()
                } else {
                    m.output.join(",")
                },
                if m.tools { " · tools" } else { " · no tools" }
            );
            let mut bench_line = vec![Span::styled(caps, t.muted())];
            if !m.benchmarks.is_empty() {
                let b = m
                    .benchmarks
                    .iter()
                    .map(|(k, v)| {
                        let label = self
                            .catalog
                            .benchmarks
                            .iter()
                            .find(|b| &b.key == k)
                            .map_or(k.clone(), |b| b.label.clone());
                        format!("{label} {}", fmt_bench(*v))
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                bench_line.push(Span::styled(format!(" · {b}"), t.muted()));
            }
            detail.extend(wrap_lines(vec![Line::from(bench_line)], w, &[], &[]));
            if let Some(warn) = self
                .catalog
                .provider(&m.provider)
                .and_then(|p| p.warning.clone())
            {
                detail.extend(wrap_lines(
                    vec![Line::from(Span::styled(
                        format!(
                            "⚠ discovery failed, using static models: {warn}{}",
                            self.catalog
                                .provider(&m.provider)
                                .and_then(|p| p.warning_detail.as_ref())
                                .map(|d| format!(" ({d}) · ^R retries"))
                                .unwrap_or_default()
                        ),
                        Style::default().fg(t.yellow),
                    ))],
                    w,
                    &[],
                    &[],
                ));
            }
        }
        let safety = self
            .safety
            .as_deref()
            .map(|id| {
                self.catalog
                    .get(id)
                    .map_or(id.to_string(), |m| m.label.clone())
            })
            .unwrap_or_else(|| "none".into());
        let enter = if self.heading.is_some() {
            "enter choose"
        } else if self.choose_safety {
            "enter safety model"
        } else {
            "enter use"
        };
        let space = if self.filter.text.is_empty() {
            "space/^O"
        } else {
            "^O"
        };
        let esc = if self.filter.text.is_empty() {
            "esc close"
        } else {
            "esc clear"
        };
        let help = if self.discovering.is_empty() {
            format!(
                "{enter} · {space} subagent · ^P provider · ^B sort · ^S safety · ^R discover · {esc} · ⛨ safety: {safety}"
            )
        } else {
            format!(
                "discovering models… · {enter} · {space} subagent · ^P provider · ^S safety · {esc}"
            )
        };
        let footer_style = if self.message.is_some() {
            Style::default().fg(t.yellow)
        } else {
            t.faint()
        };
        // The key hints wrap onto a second row at " · " rather than lose
        // their tail on narrow screens.
        let text = self.message.as_deref().unwrap_or(&help);
        let footer: Vec<Line<'static>> = if text.width() <= w {
            vec![Line::from(Span::styled(text.to_string(), footer_style))]
        } else {
            let mut cut = 0;
            for (i, _) in text.match_indices(" · ") {
                if text[..i].width() <= w {
                    cut = i;
                }
            }
            if cut == 0 {
                vec![Line::from(Span::styled(truncate(text, w), footer_style))]
            } else {
                vec![
                    Line::from(Span::styled(text[..cut].to_string(), footer_style)),
                    Line::from(Span::styled(
                        truncate(&text[cut + " · ".len()..], w),
                        footer_style,
                    )),
                ]
            }
        };
        let footer_h = footer.len() as u16;
        let detail_h = detail.len() as u16 + 1;
        let want = self.rows.len().max(1) as u16 + detail_h + footer_h + 2;
        let height = screen.height.saturating_sub(2).min(want).min(40);
        let area = Rect {
            x: screen.x + (screen.width - width) / 2,
            y: screen.y + (screen.height - height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let list_h = inner.height.saturating_sub(detail_h + footer_h) as usize;
        self.view = list_h.max(1);
        if self.pinned {
        } else if self.selected < self.scroll {
            self.scroll = self.selected.saturating_sub(1);
        } else if self.selected >= self.scroll + list_h {
            self.scroll = self.selected + 1 - list_h.max(1);
        }
        let sort_key = self.sort.map(|s| self.catalog.benchmarks[s].key.clone());
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(list_h));
        // The fixed columns take 38; one more stays free for the scrollbar.
        let label_w = w.saturating_sub(39).clamp(10, 40);
        let safety_id = self.safety.clone();
        let mut list: Vec<Line<'static>> = vec![];
        for (ri, row) in self.rows.iter().enumerate().skip(self.scroll).take(list_h) {
            match row {
                Row::Header(p) => {
                    let label = self
                        .catalog
                        .models
                        .iter()
                        .find(|m| &m.provider == p)
                        .map(|m| m.provider_label.clone())
                        .or_else(|| self.catalog.provider(p).map(|v| v.label.clone()))
                        .unwrap_or_else(|| p.clone());
                    let off = self.disabled_providers.contains(p);
                    let enabled = self.catalog.provider(p).map_or_else(
                        || {
                            self.catalog
                                .models
                                .iter()
                                .any(|m| &m.provider == p && m.provider_enabled)
                        },
                        |v| v.enabled,
                    );
                    let mut spans = vec![Span::styled(
                        format!("{label} "),
                        Style::default().fg(t.heading).add_modifier(Modifier::BOLD),
                    )];
                    if &label != p {
                        spans.push(Span::styled(format!("{p} "), t.faint()));
                    }
                    spans.push(Span::styled(
                        if off { "subagents off" } else { "subagents on" },
                        Style::default().fg(if off { t.red } else { t.fog }),
                    ));
                    if !enabled {
                        spans.push(Span::styled(
                            "  provider disabled",
                            Style::default().fg(t.yellow),
                        ));
                    }
                    let pv = self.catalog.provider(p);
                    if self.discovering.contains(p) {
                        spans.push(Span::styled("  discovering…", Style::default().fg(t.link)));
                    } else if pv.is_some_and(|p| p.warning.is_some()) {
                        spans.push(Span::styled(
                            "  ⚠ discovery failed",
                            Style::default().fg(t.yellow).add_modifier(Modifier::BOLD),
                        ));
                    }
                    list.push(Line::from(spans));
                }
                Row::Empty(p) => {
                    let pv = self.catalog.provider(p);
                    let text = if self.discovering.contains(p) {
                        "  no models yet · discovering…".to_string()
                    } else if let Some(w) = pv.and_then(|p| p.warning.as_ref()) {
                        let detail = pv
                            .and_then(|p| p.warning_detail.as_ref())
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default();
                        format!("  no models · {w}{detail} · ^R retries")
                    } else if pv.is_some_and(|p| !p.enabled) {
                        "  no models · provider disabled".to_string()
                    } else {
                        "  no models · ^R discovers".to_string()
                    };
                    list.push(Line::from(Span::styled(
                        truncate(&text, w),
                        if !self.discovering.contains(p) && pv.is_some_and(|p| p.warning.is_some())
                        {
                            Style::default().fg(t.yellow)
                        } else {
                            t.faint()
                        },
                    )));
                }
                Row::Model(i) => {
                    let m = &self.catalog.models[*i];
                    let sel = ri == self.selected;
                    let active = self.active.as_deref() == Some(m.id.as_str());
                    let (sub, sub_style) = self.sub_state(m);
                    let shield = safety_id.as_deref() == Some(m.id.as_str());
                    let ctx = m.context_window.map(fmt_tokens).unwrap_or("—".into());
                    let caps = format!(
                        "{}{}",
                        if m.output.iter().any(|o| o != "text") {
                            "media "
                        } else {
                            ""
                        },
                        if m.tools { "tools" } else { "" }
                    );
                    let bench = sort_key
                        .as_ref()
                        .and_then(|k| bench(m, k))
                        .map(fmt_bench)
                        .or_else(|| {
                            m.benchmarks
                                .first()
                                .map(|(k, v)| format!("{k} {}", fmt_bench(*v)))
                        })
                        .unwrap_or_default();
                    let base = if sel {
                        Style::default().bg(t.select_bg).fg(t.ink)
                    } else {
                        t.text()
                    };
                    let mut spans = vec![
                        Span::styled(if sel { "› " } else { "  " }, base.fg(t.accent)),
                        Span::styled(if active { "● " } else { "  " }, base.fg(t.accent)),
                        Span::styled(
                            format!("{sub} "),
                            sub_style.bg(if sel { t.select_bg } else { t.surface }),
                        ),
                        Span::styled(if shield { "⛨ " } else { "  " }, base.fg(t.link)),
                        Span::styled(
                            format!("{:<label_w$}", truncate(&m.label, label_w)),
                            if active {
                                base.add_modifier(Modifier::BOLD)
                            } else {
                                base
                            },
                        ),
                        Span::styled(format!(" {ctx:>6}"), base.fg(t.fog)),
                        Span::styled(format!(" {caps:<11}"), base.fg(t.fog)),
                        Span::styled(format!(" {bench:>10}"), base.fg(t.fog)),
                    ];
                    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                    if sel && used < w {
                        spans.push(Span::styled(" ".repeat(w - used), base));
                    }
                    list.push(Line::from(spans));
                }
            }
        }
        if self.rows.is_empty() {
            list.push(Line::from(Span::styled("no models match", t.faint())));
        }
        let list_area = Rect {
            height: list_h as u16,
            ..inner
        };
        for (n, ri) in (self.scroll..self.rows.len()).take(list_h).enumerate() {
            let row = Rect {
                y: list_area.y + n as u16,
                height: 1,
                width: list_area.width.saturating_sub(1),
                ..list_area
            };
            hits.add(row, Target::Row(ri));
            if matches!(self.rows[ri], Row::Model(_)) {
                // "› ● ✓ ⛨ label": the toggle at column 4, the badge at 6.
                hits.add(
                    Rect {
                        x: row.x + 4,
                        width: 1,
                        ..row
                    },
                    Target::Cell(ri, CELL_SUB),
                );
                hits.add(
                    Rect {
                        x: row.x + 6,
                        width: 1,
                        ..row
                    },
                    Target::Cell(ri, CELL_SAFETY),
                );
            }
        }
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::Overlay,
            Rect {
                x: inner.right().saturating_sub(1),
                width: 1,
                ..list_area
            },
            self.rows.len(),
            list_h,
            self.scroll,
        );
        frame.render_widget(Paragraph::new(list), list_area);
        let rule = Line::from(Span::styled("─".repeat(w), Style::default().fg(t.rule)));
        let mut bottom = vec![rule];
        bottom.extend(detail);
        let detail_area = Rect {
            y: inner.y + list_h as u16,
            height: detail_h,
            ..inner
        };
        frame.render_widget(Paragraph::new(bottom), detail_area);
        let footer_area = Rect {
            y: inner.y + inner.height.saturating_sub(footer_h),
            height: footer_h,
            ..inner
        };
        frame.render_widget(Paragraph::new(footer), footer_area);
    }
}

fn bench(m: &ModelInfo, key: &str) -> Option<f64> {
    m.benchmarks.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
}

/// Words a query can match: label, id, provider, description, capabilities.
fn haystack(m: &ModelInfo) -> String {
    let mut caps: Vec<&str> = vec![];
    if m.tools {
        caps.push("tools");
    }
    if !m.efforts.is_empty() {
        caps.push("reasoning effort");
    }
    if m.output.iter().any(|o| o != "text") {
        caps.push("media");
    }
    format!(
        "{} {} {} {} {} {} {} {}",
        m.label,
        m.id,
        m.provider,
        m.provider_label,
        m.description,
        m.input.join(" "),
        m.output.join(" "),
        caps.join(" ")
    )
    .to_lowercase()
}

/// Fuzzy match, per query word: a substring anywhere, or a subsequence of
/// the label or id (so `hku` finds Haiku without matching everything).
fn fuzzy(m: &ModelInfo, hay: &str, query: &str) -> bool {
    let subseq = |s: &str, word: &str| {
        let mut chars = s.chars();
        word.chars().all(|q| chars.any(|h| h == q))
    };
    let label = m.label.to_lowercase().replace(' ', "");
    let id = m.id.rsplit('/').next().unwrap_or(&m.id).to_lowercase();
    query
        .split_whitespace()
        .all(|word| hay.contains(word) || subseq(&label, word) || subseq(&id, word))
}
