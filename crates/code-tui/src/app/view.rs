//! Frame layout and drawing: transcript (with the sticky previous-prompt
//! header, selection, inner box scrollbars), the agent navigation strip,
//! goals, composer, status line, overlays. Every clickable element records
//! its rect in `self.hits` while drawing; hover is painted from the same
//! rects at the end of the frame.
use super::*;
use crate::rows::{Btn, Meta};
use crate::transcript::BoxInfo;
use crate::ui::StatusSeg;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

/// Child rows the strip shows before it scrolls.
pub(super) const STRIP_ROWS: usize = 5;
/// Finished children beyond this fold into `+ N finished`.
const FOLD_AFTER: usize = 3;

/// One row of the agent strip.
#[derive(Clone, PartialEq)]
pub(super) enum StripItem {
    Child(String),
    Fold(usize),
}

/// Transcript geometry from the last frame.
#[derive(Default, Clone)]
pub(super) struct Layout {
    pub starts: Vec<usize>,
    pub lens: Vec<usize>,
    pub total: usize,
    pub top: usize,
    pub view_h: usize,
    /// The transcript rows on screen (sticky header excluded, scrollbar
    /// column excluded).
    pub area: Rect,
}

impl Layout {
    /// The entry and its row at absolute transcript row `abs` (None on a gap).
    pub fn locate(&self, abs: usize) -> Option<(usize, usize)> {
        let i = self.starts.partition_point(|&s| s <= abs).checked_sub(1)?;
        (abs < self.starts[i] + self.lens[i]).then(|| (i, abs - self.starts[i]))
    }
    /// The entry at screen row `y`, counting the gap row below an entry as
    /// part of it (None outside the transcript rows).
    pub fn entry_near(&self, x: u16, y: u16) -> Option<usize> {
        let a = self.area;
        if y < a.y || y >= a.bottom() || x < a.x || x > a.right() {
            return None;
        }
        let abs = self.top + (y - a.y) as usize;
        if abs >= self.total {
            return None;
        }
        self.starts.partition_point(|&s| s <= abs).checked_sub(1)
    }
    /// The absolute row at screen row `y` (clamped into the view).
    pub fn abs_at(&self, y: u16) -> usize {
        let rel = (y as isize - self.area.y as isize).clamp(0, self.view_h.max(1) as isize - 1);
        (self.top + rel as usize).min(self.total.saturating_sub(1))
    }
}

/// Text selection in transcript coordinates (absolute row, display column).
#[derive(Clone, Copy, Debug)]
pub(super) struct Sel {
    pub anchor: (usize, u16),
    pub focus: (usize, u16),
    pub active: bool,
    /// Started inside this entry's inner box: limited to the box rows.
    pub boxed: Option<usize>,
}

impl Sel {
    pub fn ordered(&self) -> ((usize, u16), (usize, u16)) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
    /// Selected columns of row `abs`: [c0, c1).
    pub fn cols(&self, abs: usize) -> Option<(usize, usize)> {
        let ((r0, c0), (r1, c1)) = self.ordered();
        if abs < r0 || abs > r1 || (r0 == r1 && c0 == c1) {
            return None;
        }
        let a = if abs == r0 { c0 as usize } else { 0 };
        let b = if abs == r1 { c1 as usize } else { usize::MAX };
        Some((a, b))
    }
}

/// A status-line group: its priority and spans (with click targets).
type StatusGroup = (u8, Vec<(Span<'static>, Option<StatusSeg>)>);

fn status_icon(status: &str, tick: usize) -> (&'static str, Color) {
    let t = theme();
    match status {
        "running" | "compacting" => (SPINNER[tick % SPINNER.len()], t.yellow),
        "waiting" => ("◌", t.yellow),
        s if s.starts_with("queued") => ("⋯", t.fog),
        "done" => ("✓", t.green),
        // A roadblocked child waits for its parent to pivot, stop or override (D91).
        "roadblocked" => ("⊘", t.keyword),
        "failed" | "killed" | "interrupted" => ("✗", t.red),
        _ => ("·", t.fog),
    }
}

impl App {
    pub fn draw(&mut self, frame: &mut Frame) {
        self.links.clear();
        self.draw_frame(frame);
        // Links stay hyperlinks only where nothing was drawn over them.
        let buf = frame.buffer_mut();
        self.links.retain(|l| {
            let mut x = l.x;
            l.text.chars().all(|c| {
                let ok = buf
                    .cell((x, l.y))
                    .is_some_and(|cell| cell.symbol().starts_with(c));
                x += 1;
                ok
            })
        });
    }

    fn draw_frame(&mut self, frame: &mut Frame) {
        let t = theme();
        self.hits.clear();
        // A selection or focus belongs to one transcript generation and view.
        let owner = (self.view.clone(), self.view_transcript().generation);
        if owner != self.sel_owner {
            self.sel = None;
            self.focus = None;
            self.sel_owner = owner;
        }
        let area = frame.area();
        let width = area.width as usize;
        self.wide = area.width >= WIDE;
        let viewing = self.view_index().map(|i| {
            let a = &self.tree.agents[i];
            (
                a.label.clone(),
                alive(&a.view.status),
                crate::engine::status_text(&a.view.status),
            )
        });
        let view_placeholder = match &viewing {
            Some((label, true, _)) => {
                format!("message {label} · reaches it at its next turn boundary · esc back to root")
            }
            Some((label, false, status)) => format!("{label} is {status} · esc back to root"),
            None => String::new(),
        };
        let placeholder = if self.read_only.is_some() {
            "read-only · /fork, /new or /resume"
        } else if viewing.is_some() {
            view_placeholder.as_str()
        } else if self.running && self.composer.vim.is_some() {
            "type to queue a message · esc normal mode, esc again cancels the turn"
        } else if self.running {
            "type to queue a message · esc cancels the turn"
        } else {
            "message · / for commands · shift+enter newline"
        };
        // Attachment chips the active model can't take show in yellow (D79).
        let inputs = self.active_info().map(|m| m.input.clone());
        for b in &mut self.composer.buf.blocks {
            b.warn = match (&b.attach, &inputs) {
                (Some(k), Some(i)) => !crate::attach::supported(k, i),
                _ => false,
            };
        }
        // The composer keeps its rightmost column for the scrollbar.
        let crate::composer::BufLayout {
            rows: all_rows,
            cursor: (cx, cy),
            chips,
        } = self.composer.layout(width.saturating_sub(1), placeholder);
        // The find bar takes the composer's place while open.
        let (all_rows, cx, cy, chips) = match &self.search {
            Some(s) => {
                let (line, x) = search_bar(s, width.saturating_sub(1));
                (vec![line], x, 0, vec![])
            }
            None => (all_rows, cx, cy, chips),
        };
        let max_rows = MAX_COMPOSER_ROWS.min((area.height as usize / 3).max(1));
        let skip = (cy as usize + 1).saturating_sub(max_rows);
        let comp_total = all_rows.len();
        let crows: Vec<Line<'static>> = all_rows.into_iter().skip(skip).take(max_rows).collect();
        let queue_h = self.queue_height();
        let banner_rows = self.read_only.is_some() as u16;
        let comp_h = crows.len() as u16;
        let bottom = comp_h + 2 + queue_h + banner_rows;
        let trans_h = area.height.saturating_sub(bottom);
        let mut trans_area = Rect {
            height: trans_h,
            ..area
        };
        // The terminal sidebar (D144): a right column when wide, else it
        // covers the chat area.
        let term_panel = self.term_layout(&mut trans_area, self.wide);
        // Side column: goals only (D50).
        self.goals_area = Rect::default();
        if self.wide && self.show_goals && term_panel.is_none() {
            let side = Rect {
                x: area.x + area.width - SIDE_W,
                width: SIDE_W,
                ..trans_area
            };
            trans_area.width -= SIDE_W + 1;
            if side.height >= 3 {
                self.draw_goals(frame, side);
            }
        }
        // Agent navigation strip above the chat.
        self.strip_area = Rect::default();
        let (crumbs, items) = self.strip_items();
        if (self.view.is_some() || !items.is_empty()) && trans_area.height > 4 {
            let rows = items.len().min(STRIP_ROWS) as u16;
            let strip = Rect {
                height: 1 + rows,
                ..trans_area
            };
            trans_area.y += strip.height;
            trans_area.height -= strip.height;
            self.draw_strip(frame, strip, &crumbs, &items);
        }
        self.draw_transcript(frame, trans_area);
        let mut y = area.y + trans_h;
        if let Some(msg) = &self.read_only {
            let text = format!(" ⚠ READ-ONLY · {msg} · /fork, /new or /resume ");
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate(&text, width),
                    Style::default()
                        .fg(t.surface)
                        .bg(t.red)
                        .add_modifier(Modifier::BOLD),
                )))
                .style(Style::default().bg(t.red)),
                Rect {
                    y,
                    height: 1,
                    ..area
                },
            );
            y += 1;
        }
        self.draw_queue(
            frame,
            Rect {
                y,
                height: queue_h,
                ..area
            },
        );
        y += queue_h;
        // Rule with transient hints, or the focused entry's keys.
        let hint = match &self.hint {
            Some((h, at)) if at.elapsed() < Duration::from_secs(4) => format!(" {h} "),
            _ if self.queue_focus.is_some() => {
                " queue · ↑↓ select · s send now · t toggle · e edit · x cancel · esc leave ".into()
            }
            _ => match &self.focus {
                Some(_) => {
                    " focused · ←→ choose · enter run · ↑↓ scroll box · space toggle · esc leave "
                        .into()
                }
                None if self.strip_focus => {
                    " agents · ↑↓ select · enter open · ⌫/esc up · x kill bg · ctrl+t leave ".into()
                }
                None => String::new(),
            },
        };
        let hint = truncate(&hint, width.saturating_sub(4));
        let rule_w = width.saturating_sub(hint.width() + 2);
        // Under the queue box the rule is the box's bottom border.
        let (left, right) = if queue_h > 0 {
            (format!("╰{}", "─".repeat(rule_w.saturating_sub(1))), "─╯")
        } else {
            ("─".repeat(rule_w), "──")
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(left, Style::default().fg(t.rule)),
                Span::styled(hint, Style::default().fg(t.accent)),
                Span::styled(right, Style::default().fg(t.rule)),
            ])),
            Rect {
                y,
                height: 1,
                ..area
            },
        );
        y += 1;
        let comp_area = Rect {
            y,
            height: comp_h,
            ..area
        };
        frame.render_widget(Paragraph::new(crows), comp_area);
        self.comp_area = comp_area;
        self.comp_skip = skip;
        // Paste chips are clickable (view · edit · expand · remove).
        self.chip_rects.clear();
        for (row, x0, x1, b) in chips {
            if row < skip || row >= skip + comp_h as usize {
                continue;
            }
            let rect = Rect {
                x: area.x + x0,
                y: comp_area.y + (row - skip) as u16,
                width: x1.saturating_sub(x0),
                height: 1,
            };
            self.chip_rects.push((rect, b));
            self.hits.add(rect, Target::Chip(b));
        }
        self.hits.region(comp_area, ScrollId::Composer);
        ui::scrollbar(
            frame.buffer_mut(),
            &mut self.hits,
            ScrollId::Composer,
            comp_area,
            comp_total,
            comp_h as usize,
            skip,
        );
        if matches!(self.overlay, Overlay::None) && !self.term_typing() {
            frame.set_cursor_position((area.x + cx, y + cy - skip as u16));
        }
        y += comp_h;
        self.draw_status(
            frame,
            Rect {
                y,
                height: 1,
                ..area
            },
        );
        if let Some(panel) = term_panel {
            self.draw_terms(frame, panel);
        }
        self.draw_slash(frame, comp_area);
        self.draw_mentions(frame, comp_area);
        let waiting = self.pending().saturating_sub(1);
        if !matches!(self.overlay, Overlay::None) {
            // A modal takes every click.
            self.hits.clear();
        }
        let stats = self.cost_view_data();
        let usage = matches!(self.overlay, Overlay::Usage(_)).then(|| self.usage_data());
        match &mut self.overlay {
            Overlay::None => {}
            Overlay::ForkChat(c) => c.render(frame, area, &mut self.hits, self.tick),
            Overlay::Questions(d) => {
                let waiting = self.prompts.len().saturating_sub(d.calls.len());
                if self.prompt_title.is_empty() {
                    self.prompt_title = d.title.clone();
                }
                d.title = if waiting > 0 {
                    format!("{} · {waiting} more waiting", self.prompt_title)
                } else {
                    self.prompt_title.clone()
                };
                d.render(frame, area, &mut self.hits)
            }
            Overlay::Dialog(d, purpose) => {
                if matches!(
                    purpose,
                    DialogPurpose::Approval(_)
                        | DialogPurpose::GoalDraft
                        | DialogPurpose::Proposal(..)
                        | DialogPurpose::Roadblock(_)
                ) {
                    if self.prompt_title.is_empty() {
                        self.prompt_title = d.title.clone();
                    }
                    d.title = if waiting > 0 {
                        format!("{} · {waiting} more waiting", self.prompt_title)
                    } else {
                        self.prompt_title.clone()
                    };
                } else {
                    self.prompt_title.clear();
                }
                d.render(frame, area, &mut self.hits)
            }
            Overlay::Picker(p) => p.render(frame, area, &mut self.hits),
            Overlay::List(l, _) => l.render(frame, area, &mut self.hits),
            Overlay::Pager(p) => p.render(frame, area, &mut self.hits),
            Overlay::Cost(c) => c.render(frame, area, &mut self.hits, stats),
            Overlay::Usage(u) => {
                if let Some(data) = usage {
                    u.render(frame, area, &mut self.hits, data)
                }
            }
            Overlay::Menu(m) => m.render(frame, area, &mut self.hits),
            Overlay::Criteria(v) => v.render(frame, area, &mut self.hits),
            Overlay::Contract(ed, _) => ed.render(frame, area, &mut self.hits),
            Overlay::Diff(d) => d.render(frame, area, &mut self.hits),
            Overlay::Agents(v) => v.render(frame, area, &mut self.hits),
            Overlay::Decisions(v) => v.render(frame, area, &mut self.hits),
        }
        self.paint_hover(frame);
    }

    /// Hover: the topmost hit under the pointer gets the one hover rule
    /// (D63): chips get the wash, accent and bold; rows get a full-width
    /// wash with brightened text. A target spread over several rects (the
    /// rows of a past user message) is washed as a whole.
    fn paint_hover(&mut self, frame: &mut Frame) {
        let hit = self.mouse_pos.and_then(|(x, y)| self.hits.at(x, y));
        if let Some((rect, target)) = &hit {
            let kind = match target {
                Target::Button(..)
                | Target::Status(_)
                | Target::Cell(..)
                | Target::Finished
                | Target::Chip(_)
                | Target::Tab(_)
                | Target::QueueAct(..)
                | Target::Open(..)
                | Target::StripAct(..)
                | Target::Act(_) => Some(ui::HoverKind::Chip),
                // Breadcrumb segments are chips; strip rows are rows.
                Target::Agent(_) if rect.y == self.strip_area.y && self.strip_area.height > 0 => {
                    Some(ui::HoverKind::Chip)
                }
                Target::Scroll(_) => None,
                // The terminal screen and its edge never wash.
                Target::Term(ui::TermAct::Body | ui::TermAct::Edge) => None,
                Target::Term(_) => Some(ui::HoverKind::Chip),
                _ => Some(ui::HoverKind::Row),
            };
            match kind {
                Some(kind) => {
                    let rects: Vec<Rect> = self
                        .hits
                        .rects
                        .iter()
                        .filter(|(r, t)| t == target && (kind == ui::HoverKind::Row || r == rect))
                        .map(|(r, _)| *r)
                        .collect();
                    for r in rects {
                        ui::hover(frame.buffer_mut(), r, kind);
                    }
                }
                None => {
                    if let Target::Scroll(id) = target
                        && let Some(g) = self.hits.scroll(id)
                    {
                        ui::hover_thumb(frame.buffer_mut(), &g);
                    }
                }
            }
        }
        self.hover = hit;
    }

    /// Breadcrumb (id, label, marked) to the viewed agent, and its children.
    pub(super) fn strip_items(&self) -> (Vec<(String, String, bool)>, Vec<StripItem>) {
        let viewed = self.view.clone().unwrap_or_else(|| "root".into());
        let mut chain = vec![];
        let mut next = Some(viewed.clone());
        while let Some(id) = next {
            if chain.len() > 64 || id == "root" {
                break;
            }
            next = self.tree.get(&id).map(|a| a.view.parent.clone());
            chain.push(id);
        }
        chain.push("root".into());
        chain.reverse();
        // `!` marks the deepest crumb leading to a waiting asker outside the view.
        let askers = self.prompt_agents();
        let marked_crumb = askers
            .iter()
            .filter(|a| !self.in_subtree(a, &viewed) || **a == viewed)
            .filter_map(|a| chain.iter().rposition(|c| self.in_subtree(a, c)))
            .max();
        let crumbs = chain
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), self.tree.label(id), Some(i) == marked_crumb))
            .collect();
        let children: Vec<&crate::agents::Agent> = self
            .tree
            .agents
            .iter()
            .filter(|a| a.view.parent == viewed)
            .collect();
        let (running, finished): (Vec<_>, Vec<_>) =
            children.into_iter().partition(|a| alive(&a.view.status));
        let mut items: Vec<StripItem> = running
            .iter()
            .map(|a| StripItem::Child(a.view.id.clone()))
            .collect();
        if finished.len() > FOLD_AFTER && !self.show_finished {
            items.push(StripItem::Fold(finished.len()));
        } else {
            items.extend(finished.iter().map(|a| StripItem::Child(a.view.id.clone())));
        }
        (crumbs, items)
    }

    /// Whether `id` is `root_id` or one of its descendants.
    pub(super) fn in_subtree(&self, id: &str, root_id: &str) -> bool {
        if root_id == "root" {
            return true;
        }
        let mut next = Some(id.to_string());
        let mut n = 0;
        while let Some(cur) = next {
            if cur == root_id {
                return true;
            }
            n += 1;
            if n > 64 {
                break;
            }
            next = self.tree.get(&cur).map(|a| a.view.parent.clone());
        }
        false
    }

    /// Agents with a waiting approval or question.
    pub(super) fn prompt_agents(&self) -> Vec<String> {
        self.prompts
            .iter()
            .map(|p| match p {
                Prompt::Approval(a) => a.agent.clone(),
                Prompt::Question(q, _) => q.asker.clone(),
                Prompt::Goal(g, _) => g.agent().to_string(),
                Prompt::Guidance(..) | Prompt::Scope(..) => "root".into(),
            })
            .collect()
    }

    /// Agents blocked on a question to the user (D51).
    pub(super) fn blocked_agents(&self) -> Vec<String> {
        self.prompts
            .iter()
            .filter_map(|p| match p {
                Prompt::Question(q, _) if q.blocking => Some(q.asker.clone()),
                _ => None,
            })
            .collect()
    }

    /// Descendant count, running descendants and subtree totals of `id`.
    fn subtree(&self, id: &str) -> (usize, usize, crate::engine::Totals) {
        let mut totals = self.engine.agent_totals(id);
        let (mut n, mut running) = (0, 0);
        for a in &self.tree.agents {
            if a.view.id != id && self.in_subtree(&a.view.id, id) {
                n += 1;
                running += alive(&a.view.status) as usize;
                totals.add(&self.engine.agent_totals(&a.view.id));
            }
        }
        (n, running, totals)
    }

    fn draw_strip(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        crumbs: &[(String, String, bool)],
        items: &[StripItem],
    ) {
        let t = theme();
        self.strip_area = area;
        let bg = Style::default().bg(t.surface);
        frame.render_widget(Paragraph::new("").style(bg), area);
        // Breadcrumb row.
        let mut x = area.x;
        let mut spans = vec![Span::styled(" ‹ ", bg.fg(t.dim))];
        x += 3;
        let current = crumbs.len().saturating_sub(1);
        for (i, (id, label, marked)) in crumbs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" › ", bg.fg(t.dim)));
                x += 3;
            }
            let text = if *marked {
                format!("{label} !")
            } else {
                label.clone()
            };
            let style = if i == current {
                bg.fg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                bg.fg(t.ink)
            };
            let w = text.width() as u16;
            self.hits.add(
                Rect {
                    x,
                    y: area.y,
                    width: w.min(area.right().saturating_sub(x)),
                    height: 1,
                },
                Target::Agent(id.clone()),
            );
            spans.push(Span::styled(
                text,
                if *marked { style.fg(t.yellow) } else { style },
            ));
            x += w;
        }
        // D120: the viewed child's write scope and subscriptions.
        if let Some(id) = self.view.clone() {
            let names = |v: &[std::path::PathBuf]| {
                v.iter()
                    .map(|p| crate::transcript::rel_path(p))
                    .map(|p| if p.is_empty() { ".".to_string() } else { p })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut info = vec![];
            if let Some((paths, exclusive)) = self.engine.scopes().get(&id) {
                info.push(match (paths.is_empty(), exclusive) {
                    (true, _) => "writes nothing itself".to_string(),
                    (false, true) => format!("writes {}", names(paths)),
                    (false, false) => format!("writes {} (shared)", names(paths)),
                });
            }
            if let Some(subs) = self
                .engine
                .subscriptions()
                .get(&id)
                .filter(|s| !s.is_empty())
            {
                info.push(format!("watches {}", names(subs)));
            }
            if !info.is_empty() {
                let text = format!("  · {}", info.join(" · "));
                spans.push(Span::styled(text, bg.fg(t.dim)));
            }
        }
        let shown = items.len().min(STRIP_ROWS);
        self.strip_scroll = self.strip_scroll.min(items.len() - shown);
        let more_above = self.strip_scroll;
        let more_below = items.len() - shown - self.strip_scroll;
        let right = match (more_above, more_below) {
            (0, 0) => String::new(),
            (a, 0) => format!("↑ {a} more "),
            (0, b) => format!("↓ {b} more "),
            (a, b) => format!("↑{a} ↓{b} more "),
        };
        // D131: actions on the viewed child, right-aligned on the crumb row.
        let mut chips: Vec<(String, ui::StripAct)> = vec![];
        if let Some(id) = self.view.clone()
            && let Some(a) = self.tree.get(&id)
        {
            chips.push(("ask status".into(), ui::StripAct::Ask));
            if alive(&a.view.status) {
                if !self.engine.subagents().0 {
                    chips.push(("finalize".into(), ui::StripAct::Finalize));
                }
                chips.push(("stop branch".into(), ui::StripAct::Stop));
            }
            chips.push(("/agents".into(), ui::StripAct::Agents));
        }
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let chips_w: usize = chips.iter().map(|(l, _)| l.width() + 3).sum();
        let (chips, chips_w) = if used + chips_w + right.width() + 2 <= area.width as usize {
            (chips, chips_w)
        } else {
            (vec![], 0)
        };
        if used + chips_w + right.width() < area.width as usize {
            spans.push(Span::raw(
                " ".repeat(area.width as usize - used - chips_w - right.width()),
            ));
            let mut x = area.x + (area.width as usize - chips_w - right.width()) as u16;
            let id = self.view.clone().unwrap_or_default();
            for (label, act) in chips {
                let text = format!(" {label} ");
                let w = text.width() as u16;
                self.hits.add(
                    Rect {
                        x,
                        y: area.y,
                        width: w,
                        height: 1,
                    },
                    Target::StripAct(id.clone(), act),
                );
                let fg = if act == ui::StripAct::Stop {
                    t.red
                } else {
                    t.link
                };
                spans.push(Span::styled(text, bg.fg(fg)));
                spans.push(Span::styled(" ", bg));
                x += w + 1;
            }
            spans.push(Span::styled(right, bg.fg(t.dim)));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(bg),
            Rect { height: 1, ..area },
        );
        // Child rows.
        let rows_area = Rect {
            y: area.y + 1,
            height: shown as u16,
            ..area
        };
        let content = ui::content(rows_area);
        let w = content.width as usize;
        let askers = self.prompt_agents();
        let blocked = self.blocked_agents();
        for (n, item) in items.iter().skip(self.strip_scroll).take(shown).enumerate() {
            let y = rows_area.y + n as u16;
            let row = Rect {
                y,
                height: 1,
                ..content
            };
            let sel = self.strip_focus && self.strip_sel == self.strip_scroll + n;
            let base = if sel { bg.bg(t.select_bg) } else { bg };
            let line = match item {
                StripItem::Fold(k) => {
                    // A chip: only the label is the target.
                    let label = format!("+ {k} finished (click to show)");
                    self.hits.add(
                        Rect {
                            x: row.x + 3,
                            width: (label.width() as u16).min(row.width.saturating_sub(3)),
                            ..row
                        },
                        Target::Finished,
                    );
                    Line::from(vec![
                        Span::styled("   ", base),
                        Span::styled(label, base.fg(t.dim)),
                    ])
                }
                StripItem::Child(id) => {
                    self.hits.add(row, Target::Agent(id.clone()));
                    let Some(a) = self.tree.get(id) else { continue };
                    let finished = !alive(&a.view.status);
                    let (mut icon, color) = status_icon(&a.view.status, self.tick);
                    let waiting = blocked.iter().any(|b| self.in_subtree(b, id));
                    if waiting {
                        icon = "⧗";
                    }
                    let mark = if askers.iter().any(|b| self.in_subtree(b, id)) {
                        " !"
                    } else {
                        ""
                    };
                    let (desc, desc_running, totals) = self.subtree(id);
                    let label = format!("{}{mark}", a.label);
                    let status = if a.view.background {
                        format!("{} · bg", crate::engine::status_text(&a.view.status))
                    } else {
                        crate::engine::status_text(&a.view.status)
                    };
                    // Descendants only when there are some.
                    let tree = match (desc, desc_running) {
                        (0, _) => String::new(),
                        (d, 0) => format!("↳{d}  "),
                        (d, r) => format!("↳{d} ({r} running)  "),
                    };
                    let tail = format!("  {status:<10} {tree}{}", totals.cost());
                    let head = format!(" {icon} {label:<14}");
                    // The model column is truncated first at narrow widths.
                    let model_w = w.saturating_sub(head.width() + tail.width()).min(24);
                    let model = format!(
                        "{:<model_w$}",
                        truncate(&self.model_label(a.view.model.as_deref()), model_w)
                    );
                    let fg = if finished { t.dim } else { t.ink };
                    Line::from(vec![
                        Span::styled(
                            format!(" {icon} "),
                            base.fg(if waiting { t.yellow } else { color }),
                        ),
                        Span::styled(
                            format!("{label:<14}"),
                            base.fg(if mark.is_empty() { fg } else { t.yellow })
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(model, base.fg(if finished { t.dim } else { t.fog })),
                        Span::styled(truncate(&tail, w), base.fg(fg)),
                    ])
                }
            };
            frame.render_widget(Paragraph::new(line).style(base), row);
        }
        ui::scrollbar(
            frame.buffer_mut(),
            &mut self.hits,
            ScrollId::Strip,
            rows_area,
            items.len(),
            shown,
            self.strip_scroll,
        );
    }

    /// The criteria side panel: the viewed agent's contract (D110).
    fn draw_goals(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme();
        let inner_w = area.width.saturating_sub(5) as usize;
        let agent = self.view.clone().unwrap_or_else(|| "root".into());
        let contract = if agent == "root" {
            self.contract.clone()
        } else {
            self.engine.contract(&agent)
        };
        let mut lines: Vec<Line<'static>> = vec![];
        if agent == "root"
            && let Some(g) = &self.goal
        {
            lines.extend(wrap_lines(
                vec![Line::from(Span::styled(
                    super::goal::goal_line(g),
                    Style::default().fg(t.accent),
                ))],
                inner_w,
                &[],
                &[Span::raw("  ")],
            ));
            lines.push(Line::default());
        }
        if contract.is_empty() {
            lines.push(Line::from(Span::styled("no criteria yet", t.faint())));
        } else if !contract.objective.trim().is_empty() {
            lines.extend(wrap_lines(
                vec![Line::from(Span::styled(
                    contract.objective.clone(),
                    t.muted(),
                ))],
                inner_w,
                &[],
                &[],
            ));
            lines.push(Line::default());
        }
        let (rows, links) =
            crate::criteria::summary_lines(&contract, inner_w, &crate::engine::Section::ALL);
        let links_from = lines.len();
        lines.extend(rows);
        let view = area.height.saturating_sub(2) as usize;
        self.goals_scroll = self.goals_scroll.min(lines.len().saturating_sub(view));
        let total = lines.len();
        let lines: Vec<Line> = lines.into_iter().skip(self.goals_scroll).collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.rule))
            .title(Span::styled(
                format!(" Criteria · {} ", truncate(&self.tree.label(&agent), 20)),
                t.bold(),
            ))
            .padding(Padding::horizontal(1));
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(lines).block(block), area);
        self.goals_area = area;
        // Finding locations (D126) open in the pager.
        for l in &links {
            let row = links_from + l.row;
            if row < self.goals_scroll || row >= self.goals_scroll + view {
                continue;
            }
            let x = area.x + 2 + l.x;
            self.hits.add(
                Rect {
                    x,
                    y: area.y + 1 + (row - self.goals_scroll) as u16,
                    width: l.w.min(area.right().saturating_sub(x + 2)),
                    height: 1,
                },
                l.target(),
            );
        }
        ui::scrollbar(
            frame.buffer_mut(),
            &mut self.hits,
            ScrollId::Goals,
            Rect {
                x: area.right().saturating_sub(2),
                y: area.y + 1,
                width: 1,
                height: view as u16,
            },
            total,
            view,
            self.goals_scroll,
        );
    }

    /// The `@` popup (D118): like the slash menu, paths instead of commands.
    fn draw_mentions(&mut self, frame: &mut Frame, comp: Rect) {
        if !self.mention_open() || !self.slash_matches().is_empty() {
            return;
        }
        let t = theme();
        let items = &self.mentions.items;
        let room = (comp.y as usize).saturating_sub(2).min(10);
        if room == 0 {
            return;
        }
        let sel = self.mentions.sel.min(items.len() - 1);
        let shown = items.len().min(room);
        // The wheel scrolls the popup (D130); keys bring the selection back.
        let first = self
            .mention_top
            .unwrap_or((sel + 1).saturating_sub(shown))
            .min(items.len() - shown);
        let area = Rect {
            x: comp.x,
            y: comp.y - shown as u16 - 1,
            width: comp.width,
            height: shown as u16,
        };
        frame.render_widget(Clear, area);
        let w = (area.width as usize).saturating_sub(4);
        let lines: Vec<Line> = items
            .iter()
            .enumerate()
            .skip(first)
            .take(shown)
            .map(|(i, it)| {
                let base = if i == sel {
                    ui::chip_style(Style::default(), true)
                } else {
                    Style::default().fg(t.ink).bg(t.surface)
                };
                let style = if it.dir && i != sel {
                    base.fg(t.link)
                } else {
                    base
                };
                let label = format!("  @{}", truncate(&it.label, w.saturating_sub(2)));
                let pad = (area.width as usize).saturating_sub(label.width() + 1);
                Line::from(Span::styled(format!("{label}{}", " ".repeat(pad)), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
        for (row, i) in (first..first + shown).enumerate() {
            self.hits.add(
                Rect {
                    y: area.y + row as u16,
                    height: 1,
                    width: area.width.saturating_sub(1),
                    ..area
                },
                Target::Mention(i),
            );
        }
        self.hits.region(area, ScrollId::Mentions);
        ui::scrollbar(
            frame.buffer_mut(),
            &mut self.hits,
            ScrollId::Mentions,
            area,
            items.len(),
            shown,
            first,
        );
    }

    /// The slash menu: built-ins and skills (D109), scrolling with the
    /// selection when they outnumber the rows above the composer.
    fn draw_slash(&mut self, frame: &mut Frame, comp: Rect) {
        let t = theme();
        let matches = self.slash_matches();
        let room = (comp.y as usize).saturating_sub(2).min(12);
        if matches.is_empty() || room == 0 {
            return;
        }
        let sel = self.slash_sel.min(matches.len() - 1);
        let shown = matches.len().min(room);
        let first = self
            .slash_top
            .unwrap_or((sel + 1).saturating_sub(shown))
            .min(matches.len() - shown);
        // A full-width band above the composer: the transcript never shows
        // through beside it, and descriptions get the room.
        let width = comp.width;
        let name_w = matches
            .iter()
            .skip(first)
            .take(shown)
            .map(|(c, _)| c.width() + 1)
            .max()
            .unwrap_or(8)
            .clamp(8, 24);
        let area = Rect {
            x: comp.x,
            y: comp.y - shown as u16 - 1,
            width,
            height: shown as u16,
        };
        frame.render_widget(Clear, area);
        // One column stays free for the scrollbar.
        let desc_w = (width as usize).saturating_sub(name_w + 6);
        let lines: Vec<Line> = matches
            .iter()
            .enumerate()
            .skip(first)
            .take(shown)
            .map(|(i, (c, d))| {
                let style = if i == sel {
                    ui::chip_style(Style::default(), true)
                } else {
                    Style::default().fg(t.ink).bg(t.surface)
                };
                let name = format!("  /{}", truncate(c, name_w - 1));
                let pad = (name_w + 3).saturating_sub(name.width());
                Line::from(vec![
                    Span::styled(format!("{name}{}", " ".repeat(pad)), style),
                    Span::styled(
                        format!(" {:<desc_w$}", truncate(d, desc_w)),
                        style.fg(t.fog).remove_modifier(Modifier::BOLD),
                    ),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
        for (row, i) in (first..first + shown).enumerate() {
            self.hits.add(
                Rect {
                    y: area.y + row as u16,
                    height: 1,
                    width: area.width.saturating_sub(1),
                    ..area
                },
                Target::Slash(i),
            );
        }
        self.hits.region(area, ScrollId::Slash);
        ui::scrollbar(
            frame.buffer_mut(),
            &mut self.hits,
            ScrollId::Slash,
            area,
            matches.len(),
            shown,
            first,
        );
    }

    fn draw_status(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme();
        let sep = Span::styled(" │ ", Style::default().fg(t.rule));
        let info = self.active_info();
        let model = info
            .map(|m| m.label.clone())
            .or_else(|| self.engine.active_model())
            .unwrap_or_else(|| "no model".into());
        let effort = match (self.engine.effort(), info) {
            (Some(e), _) => e,
            (None, Some(m)) if !m.efforts.is_empty() => m
                .default_effort
                .clone()
                .map_or("default".into(), |d| format!("{d}*")),
            _ => "—".into(),
        };
        let mode = self.engine.mode();
        let mut spans: Vec<Span<'static>> = vec![];
        // Clickable segments by span index; positions are laid out at the end.
        let mut seg_at: Vec<(usize, StatusSeg)> = vec![];
        let mut push =
            |spans: &mut Vec<Span<'static>>, span: Span<'static>, seg: Option<StatusSeg>| {
                if let Some(s) = seg {
                    seg_at.push((spans.len(), s));
                }
                spans.push(span);
            };
        push(&mut spans, Span::styled(" ", t.bold()), None);
        if let Some(v) = &self.composer.vim {
            push(
                &mut spans,
                Span::styled(
                    v.label(),
                    Style::default()
                        .fg(if v.block_cursor() { t.link } else { t.green })
                        .add_modifier(Modifier::BOLD),
                ),
                None,
            );
            push(&mut spans, sep.clone(), None);
        }
        if let Some(title) = &self.title {
            push(
                &mut spans,
                Span::styled(truncate(title, 28), t.text()),
                Some(StatusSeg::Title),
            );
            push(&mut spans, sep.clone(), None);
        }
        if let Some(g) = &self.git.state {
            push(
                &mut spans,
                Span::styled(
                    g.label(),
                    Style::default().fg(if g.dirty() { t.yellow } else { t.fog }),
                ),
                Some(StatusSeg::Git),
            );
            push(&mut spans, sep.clone(), None);
        }
        // `/exclusive` locks the model (D67).
        let model = match self.engine.exclusive() {
            Some(e) if e.block_remote => format!("⊙ {model} · exclusive, local"),
            Some(_) => format!("⊙ {model} · exclusive"),
            None => model,
        };
        push(
            &mut spans,
            Span::styled(model, t.bold()),
            Some(StatusSeg::Model),
        );
        // D119: a backup is serving this turn.
        if let Some(b) = &self.backup_model {
            push(
                &mut spans,
                Span::styled(format!(" → backup {b}"), Style::default().fg(t.yellow)),
                Some(StatusSeg::Model),
            );
        }
        push(&mut spans, Span::styled(" · ", t.muted()), None);
        push(
            &mut spans,
            Span::styled(effort, t.muted()),
            Some(StatusSeg::Effort),
        );
        push(&mut spans, sep.clone(), None);
        let mut mode_text = mode.as_str().to_string();
        if matches!(mode, Mode::Auto | Mode::Unattended)
            && let Some(sm) = self.engine.safety_model()
        {
            mode_text = format!("{mode_text} · {}", self.model_label(Some(&sm)));
        }
        push(
            &mut spans,
            Span::styled(
                mode_text,
                Style::default()
                    .fg(mode_color(mode))
                    .add_modifier(Modifier::BOLD),
            ),
            Some(StatusSeg::Mode),
        );
        push(&mut spans, sep.clone(), None);
        let viewed = self.view.as_deref().and_then(|id| self.tree.get(id));
        let ctx = viewed.map_or(self.ctx, |agent| agent.ctx);
        let estimated = viewed.map_or(self.ctx_estimate, |agent| agent.ctx_estimate);
        let context_unknown = viewed.map_or(self.ctx_unknown, |agent| agent.ctx_unknown);
        let meter_info = if let Some(agent) = viewed {
            agent
                .view
                .model
                .as_deref()
                .and_then(|id| self.catalog.get(id))
        } else {
            info
        };
        let prefix =
            viewed.map_or_else(|| "ctx".to_string(), |agent| format!("ctx {}", agent.label));
        let window = ctx
            .1
            .or_else(|| meter_info.and_then(|model| model.context_window));
        let tilde = if estimated { "~" } else { "" };
        // A provider that reports no context (m7): no window is implied.
        let unknown =
            context_unknown || (ctx.1.is_none() && meter_info.is_some_and(|i| i.context_unknown));
        match window {
            _ if unknown => push(
                &mut spans,
                Span::styled(format!("{prefix} ?"), t.muted()),
                None,
            ),
            Some(w) if ctx.0 > 0 => {
                let (n, pct) = (ctx.0, ctx.0 as f64 / w.max(1) as f64 * 100.0);
                push(
                    &mut spans,
                    Span::styled(
                        format!(
                            "{prefix} {tilde}{pct:.0}% ({tilde}{}/{})",
                            fmt_tokens(n),
                            fmt_tokens(w)
                        ),
                        if pct >= 90.0 {
                            Style::default().fg(t.red).add_modifier(Modifier::BOLD)
                        } else if pct >= 70.0 {
                            Style::default().fg(t.yellow)
                        } else {
                            t.muted()
                        },
                    ),
                    None,
                )
            }
            _ => push(
                &mut spans,
                Span::styled(format!("{prefix} —"), t.muted()),
                None,
            ),
        }
        push(&mut spans, sep.clone(), None);
        push(
            &mut spans,
            Span::styled(self.engine.cost(), t.muted()),
            Some(StatusSeg::Cost),
        );
        if let Some((badge, pct)) = self.quota_badge() {
            push(&mut spans, sep.clone(), None);
            let style = if pct >= 100.0 {
                Style::default().fg(t.red).add_modifier(Modifier::BOLD)
            } else if pct >= 80.0 {
                Style::default().fg(t.yellow)
            } else {
                t.muted()
            };
            push(
                &mut spans,
                Span::styled(badge, style),
                Some(StatusSeg::Quota),
            );
        }
        // The goal marker (D93), else root's criteria count.
        if let Some(g) = &self.goal {
            let style = super::goal::goal_style(g);
            push(&mut spans, sep.clone(), None);
            push(
                &mut spans,
                Span::styled(super::goal::goal_line(g), style),
                Some(StatusSeg::Goals),
            );
        } else {
            let (met, total, _) = self.contract.counts();
            if total > 0 {
                push(&mut spans, sep.clone(), None);
                push(
                    &mut spans,
                    Span::styled(format!("criteria {met}/{total}"), t.muted()),
                    Some(StatusSeg::Goals),
                );
            }
        }
        if !self.tree.agents.is_empty() {
            let running = self.tree.running();
            push(&mut spans, sep.clone(), None);
            push(
                &mut spans,
                Span::styled(
                    {
                        let n = self.tree.agents.len();
                        if running > 0 {
                            format!("agents {running}/{n} running")
                        } else {
                            format!("{n} agent{}", if n == 1 { "" } else { "s" })
                        }
                    },
                    Style::default().fg(if running > 0 { t.yellow } else { t.fog }),
                ),
                Some(StatusSeg::Agents),
            );
        }
        // Running background jobs, so they are not forgotten (click: /jobs).
        let jobs = self.tree.jobs.iter().filter(|j| j.running()).count();
        if jobs > 0 {
            push(&mut spans, sep.clone(), None);
            push(
                &mut spans,
                Span::styled(
                    format!("⚙ {jobs} job{}", if jobs == 1 { "" } else { "s" }),
                    Style::default().fg(t.yellow),
                ),
                Some(StatusSeg::Jobs),
            );
        }
        // D144: open terminals (click: show or hide the sidebar).
        if let Some((text, waiting)) = self.term_status() {
            push(&mut spans, sep.clone(), None);
            push(
                &mut spans,
                Span::styled(
                    text,
                    if waiting {
                        Style::default().fg(t.green).add_modifier(Modifier::BOLD)
                    } else {
                        t.muted()
                    },
                ),
                Some(StatusSeg::Terminals),
            );
        }
        // D128: open decisions made on the user's behalf (click: /decisions).
        if self.decision_count > 0 {
            push(&mut spans, sep.clone(), None);
            push(
                &mut spans,
                Span::styled(
                    format!(
                        "◇ {} decision{}",
                        self.decision_count,
                        if self.decision_count == 1 { "" } else { "s" }
                    ),
                    t.faint(),
                ),
                Some(StatusSeg::Decisions),
            );
        }
        // D134: a cancel holds root's mail until the next message.
        if self.mail_held && !self.running {
            push(&mut spans, sep.clone(), None);
            push(
                &mut spans,
                Span::styled("results waiting; send a message to continue", t.faint()),
                None,
            );
        }
        if !self.blocked_agents().is_empty() {
            push(&mut spans, sep.clone(), None);
            push(
                &mut spans,
                Span::styled(
                    " waiting on you ",
                    Style::default()
                        .fg(t.surface)
                        .bg(t.red)
                        .add_modifier(Modifier::BOLD),
                ),
                Some(StatusSeg::Pending),
            );
        } else if self.pending() > 0 {
            push(&mut spans, sep.clone(), None);
            push(
                &mut spans,
                Span::styled(
                    format!("? {} pending", self.pending()),
                    Style::default()
                        .fg(t.surface)
                        .bg(t.yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Some(StatusSeg::Pending),
            );
        }
        push(&mut spans, sep.clone(), None);
        // Same shape as tool rows: "45s", then "3m12s".
        let secs = self.turn_started.elapsed().as_secs();
        let elapsed = if secs < 60 {
            secs.to_string()
        } else {
            format!("{}m{:02}", secs / 60, secs % 60)
        };
        let spin = SPINNER[self.tick % SPINNER.len()];
        let (state, color) = match (self.phase, &self.read_only) {
            (Phase::Idle, Some(_)) => ("read-only".to_string(), t.red),
            (Phase::Idle, None) => ("ready".to_string(), t.green),
            (Phase::Waiting, _) => (format!("{spin} waiting {elapsed}s"), t.yellow),
            (Phase::Thinking, _) => (format!("{spin} thinking {elapsed}s"), t.yellow),
            (Phase::Writing, _) => (format!("{spin} writing {elapsed}s"), t.yellow),
            (Phase::Tool, _) => (format!("{spin} tools {elapsed}s"), t.yellow),
            (Phase::Compacting, _) => (format!("{spin} compacting {elapsed}s"), t.yellow),
            (Phase::Retrying, _) => (format!("{spin} retrying {elapsed}s"), t.red),
            (Phase::Reconnecting, _) => (format!("{spin} reconnecting {elapsed}s"), t.yellow),
            (Phase::Cancelling, _) => (format!("{spin} cancelling"), t.red),
        };
        push(
            &mut spans,
            Span::styled(state, Style::default().fg(color)),
            None,
        );
        let queued = self.engine.queued_count();
        if queued > 0 {
            push(
                &mut spans,
                Span::styled(format!(" · {queued} queued"), t.muted()),
                None,
            );
        }
        // Groups between separators; at narrow widths the least important
        // go first (title, agents, quota, effort…), never the model, mode,
        // pending prompts or the turn state.
        let is_sep = |sp: &Span| sp.content == " │ ";
        let mut groups: Vec<StatusGroup> = vec![];
        let mut cur: Vec<(Span<'static>, Option<StatusSeg>)> = vec![];
        let n_spans = spans.len();
        for (i, sp) in spans.into_iter().enumerate() {
            let seg = seg_at.iter().find(|(k, _)| *k == i).map(|(_, s)| *s);
            if is_sep(&sp) {
                groups.push((0, std::mem::take(&mut cur)));
            } else {
                cur.push((sp, seg));
            }
            if i + 1 == n_spans {
                groups.push((0, std::mem::take(&mut cur)));
            }
        }
        let last = groups.len().saturating_sub(1);
        for (gi, (prio, items)) in groups.iter_mut().enumerate() {
            let segs: Vec<StatusSeg> = items.iter().filter_map(|(_, s)| *s).collect();
            let text: String = items.iter().map(|(sp, _)| sp.content.as_ref()).collect();
            *prio = if gi == last || segs.contains(&StatusSeg::Pending) {
                10
            } else if segs.contains(&StatusSeg::Model) {
                9
            } else if segs.contains(&StatusSeg::Mode) || segs.contains(&StatusSeg::Goals) {
                8
            } else if text.trim_start().starts_with("ctx") {
                7
            } else if segs.contains(&StatusSeg::Cost) {
                6
            } else if segs.contains(&StatusSeg::Quota) {
                5
            } else if text.contains("results waiting") {
                8
            } else if segs.contains(&StatusSeg::Terminals) {
                if self.terms.waiting() { 8 } else { 4 }
            } else if segs.contains(&StatusSeg::Agents) || segs.contains(&StatusSeg::Jobs) {
                4
            } else if segs.contains(&StatusSeg::Decisions) {
                5
            } else if segs.contains(&StatusSeg::Git) {
                2
            } else if segs.contains(&StatusSeg::Title) {
                1
            } else {
                9
            };
        }
        let right = if self.running && !self.composer.is_empty() {
            "esc clear "
        } else if self.running {
            "esc cancel "
        } else if !self.mouse {
            "mouse off · F12 "
        } else {
            "/help "
        };
        let width_of = |groups: &[StatusGroup]| -> usize {
            let body: usize = groups
                .iter()
                .map(|(_, g)| g.iter().map(|(s, _)| s.content.width()).sum::<usize>())
                .sum();
            body + groups.len().saturating_sub(1) * 3
        };
        let avail = area.width as usize;
        // The model group sheds its effort before whole groups go.
        while width_of(&groups) > avail {
            let Some((i, _)) = groups
                .iter()
                .enumerate()
                .filter(|(_, (p, _))| *p < 10)
                .min_by_key(|(i, (p, g))| {
                    // The effort sheds early, at the quota's rank.
                    let shed = *p == 9 && g.iter().any(|(s, _)| s.content == " · ");
                    (if shed { 5 } else { *p }, usize::MAX - *i)
                })
            else {
                break;
            };
            if groups[i].0 == 9
                && let Some(k) = groups[i].1.iter().position(|(s, _)| s.content == " · ")
            {
                groups[i].1.truncate(k);
                groups[i].0 = 8;
                continue;
            }
            groups.remove(i);
        }
        let mut out: Vec<Span<'static>> = vec![];
        let mut x = area.x;
        let mut segs: Vec<(u16, u16, StatusSeg)> = vec![];
        for (gi, (_, items)) in groups.into_iter().enumerate() {
            if gi > 0 {
                out.push(sep.clone());
                x += 3;
            }
            for (sp, seg) in items {
                let w = sp.content.width() as u16;
                if let Some(s) = seg {
                    segs.push((x, w, s));
                }
                x += w;
                out.push(sp);
            }
        }
        let mut spans = out;
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        if used + right.width() < area.width as usize {
            spans.push(Span::raw(
                " ".repeat(area.width as usize - used - right.width()),
            ));
            spans.push(Span::styled(right, t.faint()));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(t.surface)),
            area,
        );
        for (sx, w, seg) in segs {
            if sx < area.right() {
                self.hits.add(
                    Rect {
                        x: sx,
                        y: area.y,
                        width: w.min(area.right() - sx),
                        height: 1,
                    },
                    Target::Status(seg),
                );
            }
        }
    }

    /// The previous-prompt target: the last user message whose last row is
    /// above `top`.
    fn sticky_target(
        tr: &Transcript,
        starts: &[usize],
        lens: &[usize],
        top: usize,
    ) -> Option<usize> {
        (0..tr.entries.len())
            .filter(|&i| matches!(tr.entries[i].kind, Kind::User(_)) && lens[i] > 0)
            .rfind(|&i| starts[i] + lens[i] - 1 < top)
    }

    fn draw_transcript(&mut self, frame: &mut Frame, pane: Rect) {
        let t = theme();
        self.transcript_area = pane;
        let content = ui::content(pane);
        let width = content.width as usize;
        if width != self.layout.area.width as usize {
            // A resize rewraps everything: selection columns are stale.
            self.sel = None;
        }
        let tick = self.tick;
        let scroll = self.scroll;
        let anchor_req = self.anchor_req.take();
        let goto = self.search.as_mut().and_then(|s| s.goto.take());
        let found = self.search.as_ref().map(|s| {
            (
                s.query.to_lowercase(),
                s.cur.and_then(|c| s.matches.get(c).copied()),
            )
        });
        let tr = match self.view.as_deref().and_then(|id| self.tree.index(id)) {
            Some(i) => &mut self.tree.agents[i].transcript,
            None => &mut self.transcript,
        };
        let n = tr.entries.len();
        let gaps: Vec<bool> = (0..n).map(|i| tr.gap_before(i)).collect();
        let mut starts = Vec::with_capacity(n);
        let mut lens = Vec::with_capacity(n);
        let mut total = 0usize;
        for (i, gap) in gaps.iter().enumerate() {
            total += *gap as usize;
            starts.push(total);
            let len = tr.entries[i].height(width, tick);
            lens.push(len);
            total += len;
        }
        // Toggle anchoring: keep the clicked header on its screen row.
        let mut scroll = scroll;
        let top_for = |view_h: usize, scroll: Option<usize>| -> (usize, Option<usize>) {
            let max_top = total.saturating_sub(view_h);
            match scroll {
                Some(s) if s < max_top => (s, Some(s)),
                _ => (max_top, None),
            }
        };
        let resolve_anchor = |view_h: usize| -> Option<Option<usize>> {
            let (e, line, off) = anchor_req?;
            let max_top = total.saturating_sub(view_h) as isize;
            let want = (starts.get(e)? + line) as isize - off;
            let top = want.clamp(0, max_top);
            Some((top < max_top).then_some(top as usize))
        };
        let full_h = pane.height as usize;
        if let Some(s) = resolve_anchor(full_h) {
            scroll = s;
        }
        // A search match: centred in the view.
        if let Some((e, l)) = goto
            && let Some(st) = starts.get(e)
        {
            let max_top = total.saturating_sub(full_h);
            let want = (st + l).saturating_sub(full_h / 2);
            scroll = (want < max_top).then_some(want);
        }
        let (top_full, _) = top_for(full_h, scroll);
        let mut sticky = Self::sticky_target(tr, &starts, &lens, top_full);
        let mut area = content;
        if sticky.is_some() && pane.height > 2 {
            area.y += 1;
            area.height -= 1;
            if let Some(s) = resolve_anchor(area.height as usize) {
                scroll = s;
            }
            let (top, _) = top_for(area.height as usize, scroll);
            sticky = Self::sticky_target(tr, &starts, &lens, top).or(sticky);
        } else {
            sticky = None;
        }
        let view_h = area.height as usize;
        let (top, scroll) = top_for(view_h, scroll);
        // D103: entries more than a few screens away keep only their height.
        let margin = view_h.max(10) * 4;
        for i in 0..n {
            let (a, b) = (starts[i], starts[i] + lens[i]);
            if (b + margin < top || a > top + view_h + margin) && tr.entries[i].cached() {
                tr.entries[i].trim();
            }
        }
        let mut out: Vec<Line<'static>> = Vec::with_capacity(view_h);
        let mut row_refs: Vec<Option<(usize, usize)>> = Vec::with_capacity(view_h);
        for i in 0..n {
            let start = starts[i];
            if start > top + view_h {
                break;
            }
            if gaps[i] && start > top && start - 1 < top + view_h {
                out.push(Line::default());
                row_refs.push(None);
            }
            if start + lens[i] <= top {
                continue;
            }
            let r = tr.entries[i].render(width, tick);
            for (j, line) in r.lines.iter().enumerate() {
                let row = start + j;
                if row < top {
                    continue;
                }
                if row >= top + view_h {
                    break;
                }
                out.push(line.clone());
                row_refs.push(Some((i, j)));
            }
        }
        // Per-row metadata for hits, selection and boxes.
        let mut metas: Vec<Option<Meta>> = Vec::with_capacity(row_refs.len());
        let mut boxes: Vec<(usize, BoxInfo)> = vec![];
        for r in &row_refs {
            metas.push(r.map(|(i, j)| {
                let rendered = tr.entries[i].render(width, tick);
                if let Some(b) = rendered.boxinfo
                    && !boxes.iter().any(|(e, _)| *e == i)
                {
                    boxes.push((i, b));
                }
                rendered.meta[j].clone()
            }));
        }
        let anchored = tr.anchor();
        let task_child: Vec<Option<String>> = row_refs
            .iter()
            .map(|r| {
                r.and_then(|(i, _)| match &tr.entries[i].kind {
                    Kind::Tool(t) => t.child.as_ref().map(|c| c.id.clone()),
                    _ => None,
                })
            })
            .collect();
        let users: Vec<bool> = row_refs
            .iter()
            .map(|r| r.is_some_and(|(i, _)| matches!(tr.entries[i].kind, Kind::User(_))))
            .collect();
        let sticky_text = sticky.and_then(|i| match &tr.entries[i].kind {
            Kind::User(text) => Some(text.clone()),
            _ => None,
        });
        if scroll.is_some() && view_h > 0 {
            // Show that the view is detached from the live bottom.
            let below = total.saturating_sub(top + view_h);
            if below > 0 && out.len() == view_h {
                let label = format!(" ↓ {below} more lines · end to follow ");
                let w = label.width();
                let last = out.len() - 1;
                out[last] = Line::from(vec![
                    Span::raw(" ".repeat(width.saturating_sub(w + 1))),
                    Span::styled(label, Style::default().fg(t.ink).bg(t.select_bg)),
                ]);
                metas[last] = Some(Meta::skip());
                row_refs[last] = None;
            }
        }
        self.scroll = scroll;
        let layout_probe = Layout {
            starts: starts.clone(),
            lens: vec![],
            total,
            top,
            view_h,
            area,
        };
        frame.render_widget(Paragraph::new(out), area);
        // Hits, highlights.
        let buf = frame.buffer_mut();
        for (k, r) in row_refs.iter().enumerate() {
            let y = area.y + k as u16;
            let row = Rect {
                y,
                height: 1,
                ..area
            };
            let (Some((i, j)), Some(meta)) = (r, &metas[k]) else {
                continue;
            };
            if Some(*i) == anchored {
                buf.set_style(row, Style::default().bg(t.select_bg));
            }
            if users[k] {
                self.hits.add(row, Target::User(*i));
            }
            if meta.header {
                match &task_child[k] {
                    Some(id) => self.hits.add(row, Target::Task(id.clone())),
                    None => self.hits.add(row, Target::Header(*i, *j)),
                }
            }
            for &(a, b, btn) in &meta.buttons {
                let rect = Rect {
                    x: area.x + a,
                    width: b.saturating_sub(a),
                    ..row
                };
                if self
                    .focus
                    .is_some_and(|f| f.entry == *i && f.button == Some(btn))
                {
                    ui::hover(buf, rect.intersection(area), ui::HoverKind::Chip);
                }
                self.hits
                    .add(rect.intersection(area), Target::Button(*i, btn));
            }
            if let Some(sel) = &self.sel
                && let Some((c0, c1)) = sel.cols(top + k)
            {
                let x0 = area.x + (c0 as u16).min(area.width);
                let x1 = area.x + (c1.min(area.width as usize) as u16);
                if x1 > x0 {
                    buf.set_style(
                        Rect {
                            x: x0,
                            width: x1 - x0,
                            ..row
                        },
                        Style::default().add_modifier(Modifier::REVERSED),
                    );
                }
            }
        }
        // Search matches: every visible one tinted, the current one bold.
        if let Some((q, cur)) = &found
            && !q.is_empty()
        {
            let qn = q.chars().count();
            for (k, r) in row_refs.iter().enumerate() {
                let y = area.y + k as u16;
                let cells: Vec<(char, u16)> = (area.left()..area.right())
                    .filter_map(|x| {
                        let c = buf.cell((x, y))?.symbol().chars().next()?;
                        Some((c.to_lowercase().next().unwrap_or(c), x))
                    })
                    .collect();
                let text: Vec<char> = cells.iter().map(|(c, _)| *c).collect();
                let qv: Vec<char> = q.chars().collect();
                let current = r.is_some() && *r == *cur;
                let mut i = 0;
                while i + qn <= text.len() {
                    if text[i..i + qn] == qv[..] {
                        let x0 = cells[i].1;
                        let x1 = cells[i + qn - 1].1 + 1;
                        let style = if current {
                            Style::default()
                                .fg(t.surface)
                                .bg(t.yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(t.ink).bg(t.select_bg)
                        };
                        buf.set_style(
                            Rect {
                                x: x0,
                                width: x1 - x0,
                                y,
                                height: 1,
                            },
                            style,
                        );
                        i += qn;
                    } else {
                        i += 1;
                    }
                }
            }
        }
        // Visible URLs: click targets (hover-highlighted), then OSC 8
        // hyperlinks written over the same cells after the frame.
        let mut links = crate::links::scan(buf, area);
        for l in &mut links {
            let rect = l.rect().intersection(area);
            if self
                .mouse_pos
                .is_some_and(|(x, y)| rect.contains(ratatui::layout::Position { x, y }))
            {
                ui::hover(buf, rect, ui::HoverKind::Chip);
                if let Some(cell) = buf.cell((l.x, l.y)) {
                    (l.fg, l.bg, l.modifier) = (cell.fg, cell.bg, cell.modifier);
                }
            }
            self.hits.add(rect, Target::Url(l.url.clone()));
        }
        self.links = links;
        // Earlier text segments of a turn show their copy row only while
        // hovered, painted into the blank gap row below them (no reflow).
        self.hover_entry = self
            .mouse_pos
            .and_then(|(x, y)| layout_probe.entry_near(x, y));
        if let Some(i) = self.hover_entry
            && i + 1 < n
            && gaps[i + 1]
            && matches!(&tr.entries[i].kind, Kind::Assistant(_))
            && tr.entries[i].done
            && !tr.entries[i].turn_last
        {
            let gap_abs = starts[i + 1] - 1;
            if gap_abs >= top && gap_abs < top + view_h {
                let y = area.y + (gap_abs - top) as u16;
                let row = Rect {
                    y,
                    height: 1,
                    ..area
                };
                let (line, meta) = crate::rows::button_row(
                    "  ",
                    &[Btn::CopyText, Btn::CopyRich, Btn::CopyMarkdown],
                    Style::default().fg(t.dim),
                );
                Paragraph::new(line).render(row, buf);
                for &(a, b, btn) in &meta.buttons {
                    let rect = Rect {
                        x: area.x + a,
                        width: b.saturating_sub(a),
                        ..row
                    };
                    self.hits
                        .add(rect.intersection(area), Target::Button(i, btn));
                }
            }
        }
        if let Some(f) = &self.focus {
            // A thin marker beside the focused entry.
            for (k, r) in row_refs.iter().enumerate() {
                if r.is_some_and(|(i, _)| i == f.entry)
                    && let Some(cell) = buf.cell_mut((area.x, area.y + k as u16))
                    && cell.symbol() == " "
                {
                    cell.set_symbol("▎");
                    cell.set_fg(t.accent);
                }
            }
        }
        // Inner box scrollbars, in the column the box bodies leave free.
        for (e, info) in &boxes {
            let rows: Vec<u16> = (0..row_refs.len())
                .filter(|&k| {
                    row_refs[k].is_some_and(|(i, _)| i == *e)
                        && metas[k].as_ref().is_some_and(|m| m.boxed && !m.skip)
                })
                .map(|k| k as u16)
                .collect();
            if let (Some(&first), Some(&last)) = (rows.first(), rows.last()) {
                let track = Rect {
                    x: area.right().saturating_sub(1),
                    y: area.y + first,
                    width: 1,
                    height: last - first + 1,
                };
                ui::scrollbar(
                    buf,
                    &mut self.hits,
                    ScrollId::Box(*e),
                    track,
                    info.total,
                    info.visible,
                    info.offset,
                );
            }
        }
        // Sticky previous-prompt header.
        if let (Some(i), Some(text)) = (sticky, sticky_text) {
            let header = Rect {
                height: 1,
                ..content
            };
            let condensed = truncate(
                &crate::paste::collapse(&text)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                (header.width as usize).saturating_sub(2),
            );
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("▌", Style::default().fg(t.accent).bg(t.surface)),
                    Span::styled(
                        condensed,
                        Style::default()
                            .fg(t.ink)
                            .bg(t.surface)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]))
                .style(Style::default().bg(t.surface)),
                header,
            );
            self.hits.add(header, Target::Sticky(i));
        }
        self.sticky = sticky;
        ui::scrollbar(
            frame.buffer_mut(),
            &mut self.hits,
            ScrollId::Transcript,
            Rect {
                y: area.y,
                height: area.height,
                ..pane
            },
            total,
            view_h,
            top,
        );
        self.layout = Layout {
            starts,
            lens,
            total,
            top,
            view_h,
            area,
        };
    }

    /// Data for the cost dialog, computed fresh each frame (live numbers).
    fn cost_view_data(&self) -> CostData {
        let mut models = vec![];
        for (model, s) in &self.stats {
            let info = self.catalog.get(model);
            let label = info.map_or(model.clone(), |m| m.label.clone());
            // A subscription without rates costs nothing extra per call (m7).
            let subscription =
                info.is_some_and(|m| m.subscription) && s.unknown_calls == 0 && s.known_cost == 0.0;
            models.push((
                label,
                s.calls,
                s.input,
                s.output,
                s.cache_read,
                s.cache_write,
                if subscription {
                    "$0 (subscription)".to_string()
                } else {
                    fmt_cost(s.known_cost, s.known_calls, s.unknown_calls)
                },
                s.unknown_calls > 0,
            ));
        }
        let mut agents = vec![("root".to_string(), self.engine.agent_totals("root"))];
        // Automatic titles run on a hidden fork with their own usage,
        // recorded under the synthetic agent `auto-title` (D55).
        agents.push((
            "auto-title".to_string(),
            self.engine.agent_totals("auto-title"),
        ));
        // The goal review panel's evaluator and skeptic (D96).
        let review = self.engine.goal_review_totals();
        if review.calls > 0 || self.goal.is_some() {
            agents.push(("goal review".to_string(), review));
        }
        for (_, i) in self.tree.order() {
            let a = &self.tree.agents[i];
            agents.push((
                self.tree.path(&a.view.id),
                self.engine.agent_totals(&a.view.id),
            ));
        }
        // Quota: each provider's reported limits (live or from /usage).
        let quota = self
            .provider_usage()
            .into_iter()
            .flat_map(|u| {
                u.bars()
                    .into_iter()
                    .filter(|l| l.percent().is_some() || l.remaining.is_some())
                    .map(move |l| (u.label.clone(), l))
            })
            .collect();
        CostData {
            models,
            agents,
            total: self.engine.cost(),
            quota,
        }
    }
}

/// Rows of the cost dialog.
pub(super) struct CostData {
    /// (label, calls, input, output, cache read, cache write, cost, unknown)
    #[allow(clippy::type_complexity)]
    pub models: Vec<(String, u64, u64, u64, u64, u64, String, bool)>,
    pub agents: Vec<(String, crate::engine::Totals)>,
    pub total: String,
    /// (provider label, limit) for the quota section.
    pub quota: Vec<(String, crate::usage::Limit)>,
}

/// `/cost` modal: scrolls, updates live, Esc or q closes.
#[derive(Default)]
pub(super) struct CostView {
    pub top: usize,
    view: usize,
    len: usize,
}

impl CostView {
    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.top as isize + delta).max(0) as usize);
    }
    pub fn set_offset(&mut self, top: usize) {
        self.top = top.min(self.len.saturating_sub(self.view));
    }
    fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits, data: CostData) {
        let t = theme();
        let width = screen.width.saturating_sub(4).min(100);
        let w = width.saturating_sub(5) as usize;
        let mut lines: Vec<Line<'static>> = vec![];
        let head = |s: &str| Line::from(Span::styled(s.to_string(), t.bold()));
        let num = |n: u64| fmt_tokens(n);
        lines.push(head("By model"));
        let cost_w = data
            .models
            .iter()
            .map(|m| m.6.width() + 2)
            .max()
            .unwrap_or(0)
            .max(10);
        // Narrow screens drop the cache columns before the cost is cut.
        let with_cache = 7 + 9 * 4 + cost_w + 1;
        let show_cache = w >= with_cache + 14;
        let label_w = if show_cache {
            w.saturating_sub(with_cache).min(22)
        } else {
            w.saturating_sub(7 + 9 * 2 + cost_w + 1).clamp(8, 22)
        };
        let cache_cols = |a: &str, b: &str| {
            if show_cache {
                format!(" {a:>8} {b:>8}")
            } else {
                String::new()
            }
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{:<label_w$} {:>6} {:>8} {:>8}{} {:>cost_w$}",
                "model",
                "calls",
                "input",
                "output",
                cache_cols("cache r", "cache w"),
                "cost"
            ),
            t.faint(),
        )));
        if data.models.is_empty() {
            lines.push(Line::from(Span::styled("no model calls yet", t.faint())));
        }
        for (label, calls, i, o, cr, cw, cost, unknown) in &data.models {
            lines.push(Line::from(Span::styled(
                format!(
                    "{:<label_w$} {calls:>6} {:>8} {:>8}{} {:>cost_w$}",
                    truncate(label, label_w),
                    num(*i),
                    num(*o),
                    cache_cols(&num(*cr), &num(*cw)),
                    if *unknown {
                        format!("{cost} ?")
                    } else {
                        cost.clone()
                    }
                ),
                t.text(),
            )));
        }
        lines.push(Line::default());
        lines.push(head("By agent"));
        let crumb_w = w.saturating_sub(7 + 9 * 2 + 11).clamp(8, 38);
        lines.push(Line::from(Span::styled(
            format!(
                "{:<crumb_w$} {:>6} {:>8} {:>8} {:>10}",
                "agent", "calls", "input", "output", "cost"
            ),
            t.faint(),
        )));
        for (crumb, tot) in &data.agents {
            lines.push(Line::from(Span::styled(
                format!(
                    "{:<crumb_w$} {:>6} {:>8} {:>8} {:>10}",
                    truncate(crumb, crumb_w),
                    tot.calls,
                    num(tot.input),
                    num(tot.output),
                    tot.cost()
                ),
                t.text(),
            )));
        }
        if !data.quota.is_empty() {
            lines.push(Line::default());
            lines.push(head("Quota"));
            for (provider, l) in &data.quota {
                let mut parts = vec![];
                if let Some(p) = l.percent() {
                    parts.push(format!("{p:.0}% used"));
                }
                if let Some(a) = l.amounts() {
                    parts.push(a);
                }
                if let Some(r) = &l.resets_at {
                    parts.push(format!("resets {}", crate::usage::fmt_reset(r)));
                }
                let pct = l.percent().unwrap_or(0.0);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(
                            "{:<22} {:<16} ",
                            truncate(provider, 22),
                            truncate(&l.label, 16)
                        ),
                        t.text(),
                    ),
                    Span::styled(
                        parts.join(" · "),
                        if pct >= 100.0 {
                            Style::default().fg(t.red)
                        } else if pct >= 80.0 {
                            Style::default().fg(t.yellow)
                        } else {
                            t.muted()
                        },
                    ),
                ]));
            }
        }
        lines.push(Line::default());
        let mut total = vec![
            Span::styled("Session total ", t.bold()),
            Span::styled(data.total.clone(), Style::default().fg(t.accent)),
        ];
        // The legend only when some price is actually unknown.
        if data.models.iter().any(|m| m.7) || data.total.ends_with('+') {
            total.push(Span::styled(
                "  ($X+ means some calls had no known price; ? marks those models)",
                t.faint(),
            ));
        }
        lines.push(Line::from(total));
        let lines: Vec<Line<'static>> = lines
            .into_iter()
            .map(|l| {
                let s: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                if s.width() > w {
                    Line::from(Span::styled(truncate(&s, w), l.spans[0].style))
                } else {
                    l
                }
            })
            .collect();
        let height = (lines.len() as u16 + 4).min(screen.height.saturating_sub(2));
        let area = Rect {
            x: screen.x + (screen.width.saturating_sub(width)) / 2,
            y: screen.y + (screen.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        self.view = height.saturating_sub(4) as usize;
        self.len = lines.len();
        self.top = self.top.min(self.len.saturating_sub(self.view));
        let mut shown: Vec<Line<'static>> =
            lines.into_iter().skip(self.top).take(self.view).collect();
        shown.push(Line::default());
        shown.push(Line::from(Span::styled(
            "live · ↑↓ pgup/pgdn scroll · esc or q close",
            t.faint(),
        )));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .title(Span::styled(" Cost ", t.bold()))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(t.surface));
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        frame.render_widget(Paragraph::new(shown).block(block), area);
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
            self.len,
            self.view,
            self.top,
        );
    }
}

/// Right-click context menu on a transcript entry.
pub(super) struct Menu {
    pub x: u16,
    pub y: u16,
    pub entry: usize,
    pub items: Vec<crate::rows::Btn>,
    pub sel: usize,
}

impl Menu {
    fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let w = self
            .items
            .iter()
            .map(|b| b.label().width())
            .max()
            .unwrap_or(4) as u16
            + 6;
        let h = self.items.len() as u16 + 2;
        let x = self.x.min(screen.right().saturating_sub(w));
        let y = self.y.min(screen.bottom().saturating_sub(h));
        let area = Rect {
            x,
            y,
            width: w.min(screen.width),
            height: h.min(screen.height),
        };
        let lines: Vec<Line> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let style = if i == self.sel {
                    ui::chip_style(Style::default(), true)
                } else {
                    t.text()
                };
                // Selected rows wash the full row width, like hover.
                let inner = (w as usize).saturating_sub(4);
                Line::from(Span::styled(format!(" {:<inner$} ", b.label()), style))
            })
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .style(Style::default().bg(t.surface));
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        frame.render_widget(Paragraph::new(lines).block(block), area);
        for i in 0..self.items.len() {
            hits.add(
                Rect {
                    x: area.x + 1,
                    y: area.y + 1 + i as u16,
                    width: area.width.saturating_sub(2),
                    height: 1,
                },
                Target::Row(i),
            );
        }
    }
}

/// The find bar: `find › query   3/12 · enter older · …`, and the cursor column.
fn search_bar(s: &super::search::Search, width: usize) -> (Line<'static>, u16) {
    let t = theme();
    let head = "find › ";
    let count = if s.loading {
        "loading older context…".to_string()
    } else if s.query.is_empty() {
        String::new()
    } else if s.matches.is_empty() {
        "no matches".to_string()
    } else {
        format!("{}/{}", s.cur.map_or(0, |c| c + 1), s.matches.len())
    };
    let keys = if s.committed {
        "k/↑ older · j/↓ newer · type to edit · tab keep · esc restore"
    } else {
        "enter/↑ older · shift+enter/↓ newer · tab keep · esc restore"
    };
    let used = head.width() + s.query.width() + 3 + count.width();
    let keys = if used + 3 + keys.width() <= width {
        format!(" · {keys}")
    } else {
        String::new()
    };
    let color = if !s.query.is_empty() && s.matches.is_empty() && !s.loading {
        t.red
    } else {
        t.fog
    };
    let line = Line::from(vec![
        Span::styled(
            head,
            Style::default().fg(t.yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(s.query.clone(), t.text()),
        Span::raw("   "),
        Span::styled(count, Style::default().fg(color)),
        Span::styled(keys, t.faint()),
    ]);
    (line, (head.width() + s.query.width()) as u16)
}
