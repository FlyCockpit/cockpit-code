//! Criteria (D88–D92, D110): the read-only checklist overlay (`/criteria`)
//! and the contract editor used by `/goal` approval, `/goal edit` and a
//! roadblock pivot. Both share the dialog chrome: a rounded accent border
//! on the surface color, a faint key-hint footer, and a scrollbar in the
//! right padding column.
use crate::composer::TextBuf;
use crate::engine::{ContractDraft, ContractView, CriterionView, DraftItem, Section};
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

/// The checkbox and style of a criterion's status.
pub fn mark(c: &CriterionView) -> (&'static str, Style) {
    let t = theme();
    if c.override_note.is_some() && c.status != "met" {
        return ("◆ ", Style::default().fg(t.yellow));
    }
    match c.status.as_str() {
        "met" => ("☑ ", Style::default().fg(t.green)),
        "hit" => ("⊘ ", Style::default().fg(t.red)),
        "withdrawn" => (
            "☐ ",
            Style::default()
                .fg(t.dim)
                .add_modifier(Modifier::CROSSED_OUT),
        ),
        _ => ("☐ ", t.text()),
    }
}

/// "user · verified", "self · check failed": owner and the verification
/// label owners always see (D90). Open criteria show only the owner.
pub fn meta(c: &CriterionView, steps: bool) -> String {
    let mut parts = vec![];
    if !steps {
        parts.push(c.owner.clone());
    }
    match (c.status.as_str(), c.verification.as_str()) {
        ("met", v) => parts.push(v.replace('-', " ")),
        (_, "check-failed") => parts.push("check failed".into()),
        ("hit", _) => parts.push("hit".into()),
        ("withdrawn", _) => parts.push("withdrawn".into()),
        _ => {}
    }
    if c.checked_clean {
        parts.push("checked-clean".into());
    }
    if !c.findings.is_empty() {
        let worst = c
            .findings
            .iter()
            .map(|f| severity_rank(&f.severity))
            .min()
            .unwrap_or(9);
        parts.push(format!(
            "{} finding{}{}",
            c.findings.len(),
            if c.findings.len() == 1 { "" } else { "s" },
            SEVERITIES
                .get(worst)
                .map(|s| format!(" · worst {s}"))
                .unwrap_or_default()
        ));
    }
    if c.override_note.is_some() {
        parts.push("overridden".into());
    }
    parts.join(" · ")
}

const SEVERITIES: [&str; 5] = ["critical", "high", "medium", "low", "info"];

fn severity_rank(s: &str) -> usize {
    SEVERITIES
        .iter()
        .position(|x| x.eq_ignore_ascii_case(s))
        .unwrap_or(9)
}

fn severity_style(s: &str) -> Style {
    let t = theme();
    match severity_rank(s) {
        0 => Style::default().fg(t.red).add_modifier(Modifier::BOLD),
        1 => Style::default().fg(t.red),
        2 => Style::default().fg(t.yellow),
        3 => Style::default().fg(t.link),
        _ => t.faint(),
    }
}

/// A clickable `file:line` drawn at row `row` (of the rows returned with
/// it), columns `x..x+w`.
#[derive(Clone, Debug)]
pub struct Link {
    pub row: usize,
    pub x: u16,
    pub w: u16,
    pub path: String,
    pub line: Option<usize>,
}

impl Link {
    pub fn target(&self) -> Target {
        Target::Open(self.path.clone(), self.line)
    }
}

/// `src/a.rs:42` (or `src/a.rs:42:7`) → (path, line).
fn location(loc: &str) -> (String, Option<usize>) {
    let mut parts = loc.trim().rsplitn(3, ':').collect::<Vec<_>>();
    parts.reverse();
    match parts.as_slice() {
        [p, l, c] if l.parse::<usize>().is_ok() && c.parse::<usize>().is_ok() => {
            (p.to_string(), l.parse().ok())
        }
        [p, l] | [_, p, l] if l.parse::<usize>().is_ok() => {
            let path = loc.trim().rsplit_once(':').map_or(*p, |(a, _)| a);
            (path.to_string(), l.parse().ok())
        }
        _ => (loc.trim().to_string(), None),
    }
}

/// A row of `file:line` chips after `lead`: (line, links on it). Chips
/// that do not fit go on further rows.
fn location_rows(
    lead: &str,
    locs: &[String],
    width: usize,
    first_row: usize,
) -> (Vec<Line<'static>>, Vec<Link>) {
    let t = theme();
    let mut rows = vec![];
    let mut links = vec![];
    let mut spans = vec![Span::styled(lead.to_string(), t.faint())];
    let mut x = lead.width();
    for loc in locs {
        let text = truncate(loc.trim(), width.saturating_sub(lead.width()));
        let w = text.width();
        if x + w > width && spans.len() > 1 {
            rows.push(Line::from(std::mem::take(&mut spans)));
            spans = vec![Span::raw(" ".repeat(lead.width()))];
            x = lead.width();
        }
        let (path, line) = location(loc);
        links.push(Link {
            row: first_row + rows.len(),
            x: x as u16,
            w: w as u16,
            path,
            line,
        });
        spans.push(Span::styled(text, Style::default().fg(t.link)));
        spans.push(Span::raw("  "));
        x += w + 2;
    }
    rows.push(Line::from(spans));
    (rows, links)
}

/// D126/D132 findings of an area. Compact: one row per finding (severity,
/// first location, problem) plus coverage counts. Open: every field.
fn finding_rows(c: &CriterionView, width: usize, open: bool) -> (Vec<Line<'static>>, Vec<Link>) {
    let t = theme();
    let mut rows: Vec<Line<'static>> = vec![];
    let mut links = vec![];
    let pad = "     ";
    for f in &c.findings {
        let sev = if f.severity.is_empty() {
            "—".to_string()
        } else {
            f.severity.to_lowercase()
        };
        if !open {
            // `● high  src/a.rs:42  problem`
            let mut spans = vec![
                Span::raw(pad),
                Span::styled("● ", severity_style(&f.severity)),
                Span::styled(format!("{sev:<8} "), severity_style(&f.severity)),
            ];
            let mut x = pad.width() + 2 + sev.width().max(8) + 1;
            if let Some(loc) = f.locations.first() {
                let text = truncate(loc.trim(), (width / 2).max(12));
                let (path, line) = location(loc);
                links.push(Link {
                    row: rows.len(),
                    x: x as u16,
                    w: text.width() as u16,
                    path,
                    line,
                });
                x += text.width() + 2;
                spans.push(Span::styled(text, Style::default().fg(t.link)));
                spans.push(Span::raw("  "));
            }
            let room = width.saturating_sub(x);
            if room >= 16 {
                spans.push(Span::styled(
                    truncate(&f.text.replace('\n', " "), room),
                    t.text(),
                ));
                rows.push(Line::from(spans));
            } else {
                // Narrow (the side panel): the problem on its own row.
                rows.push(Line::from(spans));
                let deep = "       ";
                rows.push(Line::from(vec![
                    Span::raw(deep),
                    Span::styled(
                        truncate(&f.text.replace('\n', " "), width.saturating_sub(deep.len())),
                        t.text(),
                    ),
                ]));
            }
            continue;
        }
        let conf = f
            .confidence
            .map(|c| format!("  confidence {:.0}%", c * 100.0))
            .unwrap_or_default();
        rows.extend(wrap_lines(
            vec![Line::from(vec![
                Span::styled(format!("{sev} "), severity_style(&f.severity)),
                Span::styled(f.text.clone(), t.bold()),
                Span::styled(conf, t.faint()),
            ])],
            width,
            &[
                Span::raw(pad),
                Span::styled("● ", severity_style(&f.severity)),
            ],
            &[Span::raw("       ")],
        ));
        let deep = "       ";
        if !f.locations.is_empty() {
            let (lrows, mut l) =
                location_rows(&format!("{deep}at "), &f.locations, width, rows.len());
            rows.extend(lrows);
            links.append(&mut l);
        }
        for (label, text) in [
            ("fix", &f.fix),
            ("verified", &f.verification),
            ("evidence", &f.evidence),
        ] {
            let text = if text.trim().is_empty() {
                "(missing)".to_string()
            } else {
                text.trim().to_string()
            };
            for (i, l) in text.lines().enumerate() {
                rows.extend(wrap_lines(
                    vec![Line::from(vec![
                        Span::styled(format!("{:<9}", if i == 0 { label } else { "" }), t.faint()),
                        Span::styled(l.to_string(), t.text()),
                    ])],
                    width,
                    &[Span::raw(deep)],
                    &[Span::raw(format!("{deep}         "))],
                ));
            }
        }
    }
    let list = |v: &Option<Vec<String>>| match v {
        None => "not stated yet".to_string(),
        Some(v) if v.is_empty() => "none".to_string(),
        Some(v) => v.join(" · "),
    };
    let stated = c.unverified.is_some() || c.not_covered.is_some();
    if open || (stated && !c.findings.is_empty()) || c.checked_clean {
        if open {
            for (label, v) in [
                ("unverified", &c.unverified),
                ("not covered", &c.not_covered),
            ] {
                rows.extend(wrap_lines(
                    vec![Line::from(vec![
                        Span::styled(format!("{label:<12}"), t.faint()),
                        Span::styled(
                            list(v),
                            if v.is_none() {
                                Style::default().fg(t.yellow)
                            } else {
                                t.muted()
                            },
                        ),
                    ])],
                    width,
                    &[Span::raw(pad)],
                    &[Span::raw(format!("{pad}            "))],
                ));
            }
        } else {
            let n = |v: &Option<Vec<String>>| {
                v.as_ref().map_or("?".to_string(), |v| v.len().to_string())
            };
            rows.push(Line::from(vec![
                Span::raw(pad),
                Span::styled(
                    format!(
                        "unverified {} · not covered {}",
                        n(&c.unverified),
                        n(&c.not_covered)
                    ),
                    t.faint(),
                ),
            ]));
        }
    }
    (rows, links)
}

fn meta_style(c: &CriterionView) -> Style {
    let t = theme();
    match (c.status.as_str(), c.verification.as_str()) {
        ("met", "verified") => Style::default().fg(t.green),
        (_, "check-failed") | ("hit", _) => Style::default().fg(t.red),
        _ if c.override_note.is_some() => Style::default().fg(t.yellow),
        _ => t.faint(),
    }
}

fn frame_block(title: &str) -> Block<'static> {
    let t = theme();
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(Span::styled(format!(" {title} "), t.bold()))
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(t.surface))
}

fn heading(text: &str, extra: Option<String>) -> Line<'static> {
    let t = theme();
    let mut spans = vec![Span::styled(
        text.to_uppercase(),
        Style::default().fg(t.heading).add_modifier(Modifier::BOLD),
    )];
    if let Some(e) = extra {
        spans.push(Span::styled(format!("  {e}"), t.faint()));
    }
    Line::from(spans)
}

/// A criterion as rows: the checkbox and wrapped text, its meta right
/// aligned on the first row when it fits (else on a row of its own), and
/// with `open` the evidence, check and override note.
fn criterion_rows(
    c: &CriterionView,
    width: usize,
    steps: bool,
    open: bool,
    sel: bool,
) -> (Vec<Line<'static>>, Vec<Link>) {
    let t = theme();
    let (m, style) = mark(c);
    let meta_text = meta(c, steps);
    let has_more = !c.evidence.trim().is_empty()
        || c.check.is_some()
        || c.override_note.is_some()
        || !c.findings.is_empty()
        || c.unverified.is_some()
        || c.not_covered.is_some();
    let caret = match (has_more, open) {
        (false, _) => "  ",
        (true, false) => "▸ ",
        (true, true) => "▾ ",
    };
    let pointer = if sel { "›" } else { " " };
    let lead = format!("{pointer}{caret}");
    let text_style = if c.status == "withdrawn" {
        style
    } else {
        t.text()
    };
    let meta_w = meta_text.width() + 2;
    // A long meta in a narrow panel goes on its own row instead of
    // squeezing the text into a sliver.
    let text_w = if width.saturating_sub(meta_w) < width / 2 {
        width
    } else {
        width - meta_w
    };
    let mut rows = wrap_lines(
        vec![Line::from(crate::markdown::inline_code(
            &c.text, text_style,
        ))],
        text_w,
        &[
            Span::styled(lead, Style::default().fg(t.accent)),
            Span::styled(m, style.remove_modifier(Modifier::CROSSED_OUT)),
        ],
        &[Span::raw("     ")],
    );
    if !meta_text.is_empty() {
        let first_w: usize = rows[0].spans.iter().map(|s| s.content.width()).sum();
        if first_w + meta_w <= width {
            rows[0]
                .spans
                .push(Span::raw(" ".repeat(width - first_w - meta_w + 2)));
            rows[0].spans.push(Span::styled(meta_text, meta_style(c)));
        } else {
            rows.extend(wrap_lines(
                vec![Line::from(Span::styled(meta_text, meta_style(c)))],
                width,
                &[Span::raw("     ")],
                &[Span::raw("     ")],
            ));
        }
    }
    if open {
        let mut add = |label: &str, text: &str, style: Style| {
            rows.extend(wrap_lines(
                vec![Line::from(vec![
                    Span::styled(format!("{label} "), t.faint()),
                    Span::styled(text.to_string(), style),
                ])],
                width,
                &[Span::raw("     ")],
                &[Span::raw(" ".repeat(6 + label.width()))],
            ));
        };
        if !c.evidence.trim().is_empty() {
            for (i, l) in c.evidence.trim().lines().enumerate() {
                add(if i == 0 { "evidence" } else { "        " }, l, t.text());
            }
        }
        if let Some(k) = &c.check {
            add("check", k, Style::default().fg(t.code_fg));
        }
        if let Some(o) = &c.override_note {
            add("override", o, Style::default().fg(t.yellow));
        }
    }
    let (frows, mut links) = finding_rows(c, width, open);
    for l in &mut links {
        l.row += rows.len();
    }
    rows.extend(frows);
    if sel {
        for r in &mut rows {
            let w: usize = r.spans.iter().map(|s| s.content.width()).sum();
            if w < width {
                r.spans.push(Span::raw(" ".repeat(width - w)));
            }
            *r = r.clone().style(Style::default().bg(t.select_bg));
        }
    }
    (rows, links)
}

/// Lines of a contract for a markdown-free summary (dialog bodies, the
/// side panel): sections with checkboxes and meta.
pub fn summary_lines(
    c: &ContractView,
    width: usize,
    sections: &[Section],
) -> (Vec<Line<'static>>, Vec<Link>) {
    let mut out = vec![];
    let mut links = vec![];
    for &s in sections {
        let list = c.list(s);
        if list.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Line::default());
        }
        let extra = (matches!(s, Section::Acceptance | Section::Findings)).then(|| {
            let met = list.iter().filter(|c| c.status == "met").count();
            let total = list.iter().filter(|c| c.status != "withdrawn").count();
            let verified = list
                .iter()
                .filter(|c| c.status == "met" && c.verification == "verified")
                .count();
            format!("{met}/{total} met · {verified} verified")
        });
        out.push(heading(s.title(), extra));
        for item in list {
            let (rows, l) = criterion_rows(item, width, s == Section::Steps, false, false);
            links.extend(l.into_iter().map(|mut l| {
                l.row += out.len();
                l
            }));
            out.extend(rows);
        }
    }
    (out, links)
}

pub enum CriteriaAction {
    None,
    Close,
    /// Edit the user-owned criteria (root, through `/goal edit`).
    Edit,
    /// Toggle the side panel.
    Pin,
}

/// `/criteria`: the viewed agent's contract as a checklist.
pub struct CriteriaView {
    pub agent: String,
    pub label: String,
    pub contract: ContractView,
    /// The goal's one-line state, for root under `/goal`.
    /// The goal marker line and its status-line style.
    pub goal_line: Option<(String, Style)>,
    /// `e` edits (root with an editable goal).
    pub editable: bool,
    /// Selected criterion (index into `ids`).
    sel: usize,
    ids: Vec<String>,
    expanded: BTreeSet<String>,
    top: usize,
    view: usize,
    len: usize,
    /// Line span of each criterion from the last render: (first, end).
    spans: Vec<(usize, usize)>,
    /// The selection moved: scroll it into view at the next render.
    follow: bool,
}

impl CriteriaView {
    pub fn new(agent: String, label: String, contract: ContractView) -> Self {
        let mut v = Self {
            agent,
            label,
            contract: ContractView::default(),
            goal_line: None,
            editable: false,
            sel: 0,
            ids: vec![],
            expanded: BTreeSet::new(),
            top: 0,
            view: 1,
            len: 0,
            spans: vec![],
            follow: true,
        };
        v.set_contract(contract);
        v
    }
    /// A live update: the selection stays on its criterion.
    pub fn set_contract(&mut self, contract: ContractView) {
        let keep = self.ids.get(self.sel).cloned();
        self.ids = Section::ALL
            .iter()
            .flat_map(|s| contract.list(*s).iter().map(|c| c.id.clone()))
            .collect();
        self.sel = keep
            .and_then(|id| self.ids.iter().position(|i| *i == id))
            .unwrap_or(0);
        self.contract = contract;
    }
    fn toggle(&mut self, i: usize) {
        if let Some(id) = self.ids.get(i).cloned()
            && !self.expanded.remove(&id)
        {
            self.expanded.insert(id);
        }
    }
    /// A click on criterion `i`: select it and fold or unfold it.
    pub fn click(&mut self, i: usize) {
        self.sel = i.min(self.ids.len().saturating_sub(1));
        self.toggle(i);
    }
    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.top as isize + delta).max(0) as usize);
    }
    pub fn set_offset(&mut self, top: usize) {
        self.top = top.min(self.len.saturating_sub(self.view));
        self.follow = false;
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> CriteriaAction {
        let n = self.ids.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return CriteriaAction::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return CriteriaAction::Close;
            }
            KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                self.sel = self.sel.saturating_sub(1);
                self.follow = true;
            }
            KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                self.sel = (self.sel + 1).min(n - 1);
                self.follow = true;
            }
            KeyCode::PageUp => self.wheel(-(self.view.max(1) as isize)),
            KeyCode::PageDown => self.wheel(self.view.max(1) as isize),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Left => {
                let open = self
                    .ids
                    .get(self.sel)
                    .is_some_and(|id| self.expanded.contains(id));
                let want = match key.code {
                    KeyCode::Right => true,
                    KeyCode::Left => false,
                    _ => !open,
                };
                if want != open {
                    self.toggle(self.sel);
                }
                self.follow = true;
            }
            KeyCode::Char('a') => {
                // Expand or collapse every criterion.
                if self.expanded.len() < self.ids.len() {
                    self.expanded = self.ids.iter().cloned().collect();
                } else {
                    self.expanded.clear();
                }
            }
            KeyCode::Char('e') if self.editable => return CriteriaAction::Edit,
            KeyCode::Char('p') => return CriteriaAction::Pin,
            _ => {}
        }
        CriteriaAction::None
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let width = screen.width.saturating_sub(4).clamp(30, 100);
        let w = width.saturating_sub(5) as usize;
        let c = &self.contract;
        let mut lines: Vec<Line<'static>> = vec![];
        self.spans.clear();
        if let Some((g, style)) = &self.goal_line {
            lines.push(Line::from(Span::styled(g.clone(), *style)));
            lines.push(Line::default());
        }
        if c.is_empty() {
            lines.extend(wrap_lines(
                vec![Line::from(Span::styled(
                    format!(
                        "{} has no criteria yet. Agents record them with the criteria tool; /goal <text> drafts a contract for root.",
                        self.label
                    ),
                    t.faint(),
                ))],
                w,
                &[],
                &[],
            ));
        }
        if !c.objective.trim().is_empty() {
            lines.push(heading(
                "Objective",
                c.review
                    .then(|| "review panel checks the claim".to_string()),
            ));
            lines.extend(wrap_lines(
                vec![Line::from(Span::styled(c.objective.clone(), t.text()))],
                w,
                &[Span::raw("  ")],
                &[Span::raw("  ")],
            ));
        }
        let mut k = 0;
        let mut links: Vec<Link> = vec![];
        for s in Section::ALL {
            let list = c.list(s);
            if list.is_empty() {
                continue;
            }
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let extra = match s {
                Section::Acceptance | Section::Findings => {
                    let met = list.iter().filter(|c| c.status == "met").count();
                    let total = list.iter().filter(|c| c.status != "withdrawn").count();
                    let verified = list
                        .iter()
                        .filter(|c| c.status == "met" && c.verification == "verified")
                        .count();
                    Some(format!("{met}/{total} met · {verified} verified"))
                }
                Section::Steps => {
                    let done = list.iter().filter(|c| c.status == "met").count();
                    Some(format!("{done}/{} done", list.len()))
                }
                _ => None,
            };
            lines.push(heading(s.title(), extra));
            for item in list {
                let first = lines.len();
                let open = self.expanded.contains(&item.id);
                let (rows, l) = criterion_rows(item, w, s == Section::Steps, open, k == self.sel);
                links.extend(l.into_iter().map(|mut l| {
                    l.row += first;
                    l
                }));
                lines.extend(rows);
                self.spans.push((first, lines.len()));
                k += 1;
            }
        }
        if !c.proposals.is_empty() {
            let open = c.open_proposals().count();
            lines.push(Line::default());
            lines.push(heading(
                "Proposals",
                Some(format!(
                    "{open} open · {} resolved",
                    c.proposals.len() - open
                )),
            ));
            for p in &c.proposals {
                let done = p.status != "open";
                let on = c
                    .criterion(&p.criterion_id)
                    .map_or(p.criterion_id.clone(), |c| truncate(&c.text, 40));
                let mut spans = vec![
                    Span::styled(format!("“{on}” → "), t.faint()),
                    Span::styled(
                        p.change.clone(),
                        Style::default().fg(if done { t.fog } else { t.yellow }),
                    ),
                    Span::styled(format!(" — {}", p.reason), t.muted()),
                ];
                if done {
                    let how = match &p.resolution {
                        Some(r) if !r.trim().is_empty() => format!("{}: {}", p.status, r.trim()),
                        _ => p.status.clone(),
                    };
                    spans.push(Span::styled(format!(" · {how}"), t.faint()));
                }
                lines.extend(wrap_lines(
                    vec![Line::from(spans)],
                    w,
                    &[Span::styled(
                        if done { "  ✓ " } else { "  ↻ " },
                        Style::default().fg(if done { t.dim } else { t.yellow }),
                    )],
                    &[Span::raw("    ")],
                ));
            }
        }
        if !c.roadblocks.is_empty() {
            lines.push(Line::default());
            lines.push(heading("Roadblocks", None));
            for r in &c.roadblocks {
                let state = match (&r.resolution, &r.note) {
                    (None, _) => "open".to_string(),
                    (Some(res), Some(n)) if !n.trim().is_empty() => format!("{res}: {n}"),
                    (Some(res), _) => res.clone(),
                };
                let open = r.resolution.is_none();
                lines.extend(wrap_lines(
                    vec![Line::from(vec![
                        Span::styled(
                            r.reason.clone(),
                            if open {
                                Style::default().fg(t.red)
                            } else {
                                t.muted()
                            },
                        ),
                        Span::styled(format!(" · {state}"), t.faint()),
                    ])],
                    w,
                    &[Span::styled(
                        if open { "  ⊘ " } else { "  ✓ " },
                        Style::default().fg(if open { t.red } else { t.dim }),
                    )],
                    &[Span::raw("     ")],
                ));
            }
        }
        let footer = format!(
            "↑↓ select · enter evidence · a all{} · p side panel · esc close",
            if self.editable { " · e edit" } else { "" }
        );
        let max_h = screen.height.saturating_sub(2);
        let height = (lines.len() as u16 + 4).min(max_h).max(6);
        let area = Rect {
            x: screen.x + (screen.width.saturating_sub(width)) / 2,
            y: screen.y + (screen.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        self.view = height.saturating_sub(4) as usize;
        self.len = lines.len();
        if self.follow
            && let Some(&(a, b)) = self.spans.get(self.sel)
        {
            if a < self.top {
                self.top = a.saturating_sub(1);
            } else if b > self.top + self.view {
                self.top = (b - self.view.min(b)).min(a);
            }
        }
        self.top = self.top.min(self.len.saturating_sub(self.view));
        let mut shown: Vec<Line<'static>> =
            lines.into_iter().skip(self.top).take(self.view).collect();
        while shown.len() < self.view {
            shown.push(Line::default());
        }
        shown.push(Line::default());
        shown.push(Line::from(Span::styled(truncate(&footer, w), t.faint())));
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        frame.render_widget(
            Paragraph::new(shown).block(frame_block(&format!("Criteria · {}", self.label))),
            area,
        );
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: self.view as u16,
        };
        for (i, &(a, b)) in self.spans.iter().enumerate() {
            let (a, b) = (a.max(self.top), b.min(self.top + self.view));
            if a >= b {
                continue;
            }
            hits.add(
                Rect {
                    y: inner.y + (a - self.top) as u16,
                    height: (b - a) as u16,
                    ..inner
                },
                Target::Row(i),
            );
        }
        // Finding locations open in the pager (after the rows: on top).
        for l in &links {
            if l.row < self.top || l.row >= self.top + self.view {
                continue;
            }
            hits.add(
                Rect {
                    x: inner.x + l.x,
                    y: inner.y + (l.row - self.top) as u16,
                    width: l.w.min(inner.width.saturating_sub(l.x)),
                    height: 1,
                },
                l.target(),
            );
        }
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::Overlay,
            Rect {
                x: area.right().saturating_sub(2),
                width: 1,
                ..inner
            },
            self.len,
            self.view,
            self.top,
        );
    }
}

// ---- the contract editor ---------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ERow {
    Objective,
    Item(Section, usize),
    Add(Section),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Text,
    Check,
}

pub enum EditorAction {
    None,
    Cancel,
    Save,
    /// Open the draft as YAML in `$EDITOR`.
    External,
}

/// The editable contract form: every item is a row; Enter edits its text,
/// `c` its check, `d` deletes, the "+ add" rows append. Ctrl+S saves.
pub struct ContractEditor {
    pub title: String,
    /// "approve", "save" or "pivot".
    pub verb: &'static str,
    pub draft: ContractDraft,
    /// The lists this form edits (a pivot hides steps; the agent owns them).
    sections: Vec<Section>,
    sel: usize,
    editing: Option<(Field, TextBuf)>,
    /// A just-added item: dropped again when its first edit is cancelled.
    added: bool,
    pub message: Option<String>,
    top: usize,
    view: usize,
    len: usize,
    spans: Vec<(usize, usize)>,
    /// The wheel or scrollbar moved the view: leave it until the next key
    /// or click (D130).
    pinned: bool,
}

impl ContractEditor {
    pub fn new(title: impl Into<String>, verb: &'static str, draft: ContractDraft) -> Self {
        Self {
            title: title.into(),
            verb,
            draft,
            sections: Section::ALL.to_vec(),
            sel: 0,
            editing: None,
            added: false,
            message: None,
            top: 0,
            view: 1,
            len: 0,
            spans: vec![],
            pinned: false,
        }
    }
    pub fn without_steps(mut self) -> Self {
        self.sections.retain(|s| *s != Section::Steps);
        self
    }
    fn rows(&self) -> Vec<ERow> {
        let mut rows = vec![ERow::Objective];
        for &s in &self.sections {
            for i in 0..self.draft.list(s).len() {
                rows.push(ERow::Item(s, i));
            }
            rows.push(ERow::Add(s));
        }
        rows
    }
    fn text_of(&self, row: ERow, field: Field) -> String {
        match (row, field) {
            (ERow::Objective, _) => self.draft.objective.clone(),
            (ERow::Item(s, i), Field::Text) => self.draft.list(s)[i].text.clone(),
            (ERow::Item(s, i), Field::Check) => {
                self.draft.list(s)[i].check.clone().unwrap_or_default()
            }
            (ERow::Add(_), _) => String::new(),
        }
    }
    fn begin(&mut self, field: Field) {
        let rows = self.rows();
        let Some(&row) = rows.get(self.sel) else {
            return;
        };
        let row = match row {
            ERow::Add(s) => {
                self.draft.list_mut(s).push(DraftItem::default());
                self.added = true;
                let i = self.draft.list(s).len() - 1;
                self.sel = self
                    .rows()
                    .iter()
                    .position(|r| *r == ERow::Item(s, i))
                    .unwrap_or(self.sel);
                ERow::Item(s, i)
            }
            r => r,
        };
        if row == ERow::Objective && field == Field::Check {
            return;
        }
        let mut buf = TextBuf::default();
        buf.set(self.text_of(row, field));
        buf.end();
        self.editing = Some((field, buf));
    }
    fn commit(&mut self) {
        let Some((field, buf)) = self.editing.take() else {
            return;
        };
        let rows = self.rows();
        let text = buf.text.trim().to_string();
        match (rows.get(self.sel).copied(), field) {
            (Some(ERow::Objective), _) => self.draft.objective = text,
            (Some(ERow::Item(s, i)), Field::Text) => {
                if text.is_empty() && self.added {
                    self.draft.list_mut(s).remove(i);
                } else if !text.is_empty() {
                    self.draft.list_mut(s)[i].text = text;
                }
            }
            (Some(ERow::Item(s, i)), Field::Check) => {
                self.draft.list_mut(s)[i].check = (!text.is_empty()).then_some(text);
            }
            _ => {}
        }
        self.added = false;
    }
    fn cancel_edit(&mut self) {
        self.editing = None;
        if self.added
            && let Some(ERow::Item(s, i)) = self.rows().get(self.sel).copied()
            && self.draft.list(s)[i].text.is_empty()
        {
            self.draft.list_mut(s).remove(i);
        }
        self.added = false;
    }
    /// A paste goes into the field being edited (one line).
    pub fn paste(&mut self, text: &str) {
        if let Some((_, buf)) = &mut self.editing {
            buf.insert(&text.replace('\n', " "));
        }
    }
    /// A click on row `i`: select it; a second click edits.
    pub fn click(&mut self, i: usize) {
        self.pinned = false;
        if self.editing.is_some() {
            self.commit();
        }
        if self.sel == i {
            self.begin(Field::Text);
        } else {
            self.sel = i.min(self.rows().len().saturating_sub(1));
        }
    }
    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.top as isize + delta).max(0) as usize);
    }
    pub fn set_offset(&mut self, top: usize) {
        self.top = top.min(self.len.saturating_sub(self.view));
        self.pinned = true;
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> EditorAction {
        self.pinned = false;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.message = None;
        if ctrl && key.code == KeyCode::Char('s') {
            self.commit();
            return match self.draft.problem() {
                Some(p) if self.verb != "pivot" => {
                    self.message = Some(format!("can't {}: {p}", self.verb));
                    EditorAction::None
                }
                _ => EditorAction::Save,
            };
        }
        if ctrl && key.code == KeyCode::Char('g') {
            self.commit();
            return EditorAction::External;
        }
        if let Some((_, buf)) = &mut self.editing {
            match key.code {
                KeyCode::Esc => self.cancel_edit(),
                KeyCode::Enter => self.commit(),
                KeyCode::Char('c') if ctrl => self.cancel_edit(),
                KeyCode::Char('u') if ctrl => buf.kill_line(),
                KeyCode::Char('w') if ctrl => buf.delete_word(),
                KeyCode::Char(c) if !ctrl => buf.insert(c.encode_utf8(&mut [0; 4])),
                KeyCode::Backspace => buf.backspace(),
                KeyCode::Delete => buf.delete(),
                KeyCode::Left if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                    buf.word_left()
                }
                KeyCode::Right if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                    buf.word_right()
                }
                KeyCode::Left => buf.left(),
                KeyCode::Right => buf.right(),
                KeyCode::Home => buf.home(),
                KeyCode::End => buf.end(),
                _ => {}
            }
            return EditorAction::None;
        }
        let rows = self.rows();
        let n = rows.len();
        match key.code {
            KeyCode::Esc => return EditorAction::Cancel,
            KeyCode::Char('c') if ctrl => return EditorAction::Cancel,
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(n - 1),
            KeyCode::PageUp => self.wheel(-(self.view.max(1) as isize)),
            KeyCode::PageDown => self.wheel(self.view.max(1) as isize),
            KeyCode::Enter => self.begin(Field::Text),
            KeyCode::Char('c') if matches!(rows.get(self.sel), Some(ERow::Item(..))) => {
                self.begin(Field::Check)
            }
            KeyCode::Char('a') => {
                // Add to the section under the cursor.
                let s = match rows.get(self.sel) {
                    Some(ERow::Item(s, _) | ERow::Add(s)) => *s,
                    _ => Section::Acceptance,
                };
                self.sel = rows
                    .iter()
                    .position(|r| *r == ERow::Add(s))
                    .unwrap_or(self.sel);
                self.begin(Field::Text);
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(ERow::Item(s, i)) = rows.get(self.sel).copied() {
                    self.draft.list_mut(s).remove(i);
                    self.sel = self.sel.min(self.rows().len() - 1);
                }
            }
            _ => {}
        }
        EditorAction::None
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let width = screen.width.saturating_sub(4).clamp(30, 100);
        let w = width.saturating_sub(5) as usize;
        let rows = self.rows();
        let mut lines: Vec<Line<'static>> = vec![];
        self.spans.clear();
        let mut cursor = None;
        let mut last_section = None;
        for (k, row) in rows.iter().enumerate() {
            let sel = k == self.sel;
            let section = match row {
                ERow::Objective => None,
                ERow::Item(s, _) | ERow::Add(s) => Some(*s),
            };
            if section != last_section {
                lines.push(Line::default());
                if let Some(s) = section {
                    lines.push(heading(s.title(), None));
                }
                last_section = section;
            }
            if *row == ERow::Objective {
                lines.push(heading("Objective", None));
            }
            let first = lines.len();
            let editing = self.editing.as_ref().filter(|_| sel);
            let pointer =
                Span::styled(if sel { "› " } else { "  " }, Style::default().fg(t.accent));
            match row {
                ERow::Add(s) => {
                    let label = format!("+ add {}", s.noun());
                    lines.push(Line::from(vec![
                        pointer,
                        Span::styled(
                            label,
                            Style::default().fg(if sel { t.accent } else { t.dim }),
                        ),
                    ]));
                }
                _ => {
                    let item = match row {
                        ERow::Item(s, i) => Some(&self.draft.list(*s)[*i]),
                        _ => None,
                    };
                    let bullet = if item.is_some() { "• " } else { "" };
                    let lead = vec![pointer.clone(), Span::styled(bullet, t.faint())];
                    let lead_w: usize = lead.iter().map(|s| s.content.width()).sum();
                    let cont = Span::raw(" ".repeat(lead_w));
                    match editing {
                        Some((Field::Text, buf)) => {
                            let (rows_e, (cx, cy)) = buf.layout(
                                w,
                                Span::styled(
                                    format!("{}✎ ", " ".repeat(lead_w.saturating_sub(2))),
                                    Style::default().fg(t.accent),
                                ),
                                cont.clone(),
                                t.text(),
                            );
                            cursor = Some((cx, (lines.len() + cy as usize) as u16));
                            lines.extend(rows_e);
                        }
                        _ => {
                            let text = match row {
                                ERow::Objective => self.draft.objective.clone(),
                                _ => item.map(|i| i.text.clone()).unwrap_or_default(),
                            };
                            let (text, style) = if text.is_empty() {
                                ("(empty)".to_string(), t.faint())
                            } else {
                                (text, t.text())
                            };
                            let mut prefix = lead.clone();
                            if item.is_some_and(|i| i.id.is_none()) {
                                prefix.push(Span::styled("new ", Style::default().fg(t.green)));
                            }
                            lines.extend(wrap_lines(
                                vec![Line::from(Span::styled(text, style))],
                                w,
                                &prefix,
                                std::slice::from_ref(&cont),
                            ));
                        }
                    }
                    if let Some(item) = item {
                        let ind = " ".repeat(lead_w + 2);
                        match editing {
                            Some((Field::Check, buf)) => {
                                let (rows_e, (cx, cy)) = buf.layout(
                                    w,
                                    Span::styled(
                                        format!("{ind}check ✎ "),
                                        Style::default().fg(t.accent),
                                    ),
                                    Span::raw(" ".repeat(ind.len() + 8)),
                                    Style::default().fg(t.code_fg),
                                );
                                cursor = Some((cx, (lines.len() + cy as usize) as u16));
                                lines.extend(rows_e);
                            }
                            _ => {
                                if let Some(c) = &item.check {
                                    lines.extend(wrap_lines(
                                        vec![Line::from(vec![
                                            Span::styled("check ", t.faint()),
                                            Span::styled(c.clone(), Style::default().fg(t.code_fg)),
                                        ])],
                                        w,
                                        &[Span::raw(ind.clone())],
                                        &[Span::raw(" ".repeat(ind.len() + 6))],
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            if sel && editing.is_none() {
                for l in &mut lines[first..] {
                    let lw: usize = l.spans.iter().map(|s| s.content.width()).sum();
                    if lw < w {
                        l.spans.push(Span::raw(" ".repeat(w - lw)));
                    }
                    *l = l.clone().style(Style::default().bg(t.select_bg));
                }
            }
            self.spans.push((first, lines.len()));
        }
        if lines.first().is_some_and(|l| l.spans.is_empty()) {
            lines.remove(0);
            for s in &mut self.spans {
                s.0 -= 1;
                s.1 -= 1;
            }
            if let Some((_, y)) = &mut cursor {
                *y -= 1;
            }
        }
        let footer = if self.editing.is_some() {
            "enter keep · esc discard · ctrl+s keep and ".to_string() + self.verb
        } else {
            format!(
                "enter edit · c check · a add · d delete · ^G $EDITOR · ^S {} · esc",
                self.verb
            )
        };
        let height = (lines.len() as u16 + 4 + self.message.is_some() as u16)
            .min(screen.height.saturating_sub(2))
            .max(8);
        let area = Rect {
            x: screen.x + (screen.width.saturating_sub(width)) / 2,
            y: screen.y + (screen.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        self.view = height.saturating_sub(4 + self.message.is_some() as u16) as usize;
        self.len = lines.len();
        // Keep the selected row (or the cursor) in view, unless the wheel
        // moved the view.
        if !self.pinned
            && let Some(&(a, b)) = self.spans.get(self.sel)
        {
            let focus = cursor.map(|(_, y)| y as usize);
            let (a, b) = match focus {
                Some(y) => (y, y + 1),
                None => (a, b),
            };
            if a < self.top {
                self.top = a.saturating_sub(1);
            } else if b > self.top + self.view {
                self.top = (b - self.view.min(b)).min(a);
            }
        }
        self.top = self.top.min(self.len.saturating_sub(self.view));
        let mut shown: Vec<Line<'static>> =
            lines.into_iter().skip(self.top).take(self.view).collect();
        while shown.len() < self.view {
            shown.push(Line::default());
        }
        shown.push(Line::default());
        if let Some(m) = &self.message {
            shown.push(Line::from(Span::styled(
                truncate(m, w),
                Style::default().fg(t.yellow),
            )));
        }
        shown.push(Line::from(Span::styled(truncate(&footer, w), t.faint())));
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        frame.render_widget(Paragraph::new(shown).block(frame_block(&self.title)), area);
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: self.view as u16,
        };
        for (i, &(a, b)) in self.spans.iter().enumerate() {
            let (a, b) = (a.max(self.top), b.min(self.top + self.view));
            if a >= b {
                continue;
            }
            hits.add(
                Rect {
                    y: inner.y + (a - self.top) as u16,
                    height: (b - a) as u16,
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
                ..inner
            },
            self.len,
            self.view,
            self.top,
        );
        if let Some((x, y)) = cursor
            && (y as usize) >= self.top
            && (y as usize) < self.top + self.view
        {
            frame.set_cursor_position((inner.x + x, inner.y + (y as usize - self.top) as u16));
        }
    }
}
