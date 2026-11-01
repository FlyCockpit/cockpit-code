//! `/usage` (D85): every provider's limit bars, reset times, credits and
//! soft budget (D87) in one scrolling dialog. It reads in the background at
//! startup; `r` refreshes. Provider headers are clickable and fold their
//! details.
use crate::theme::theme;
use crate::ui::{self, Hits, ScrollId, Target};
use crate::usage::{Limit, ProviderUsage, fmt_reset};
use crate::wrap::truncate;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use std::collections::BTreeSet;
use unicode_width::UnicodeWidthStr;

/// What the dialog shows, computed fresh each frame.
pub(super) struct UsageData {
    pub providers: Vec<ProviderUsage>,
    pub refreshing: bool,
    /// The active model's provider (marked, listed first).
    pub active: Option<String>,
}

#[derive(Default)]
pub(super) struct UsageView {
    pub top: usize,
    view: usize,
    len: usize,
    /// Folded providers.
    folded: BTreeSet<String>,
    /// Provider of each header line, by line index.
    headers: Vec<(usize, String)>,
}

/// The color of a share: red at 100%, yellow from 80% (D87's warnings).
fn share_style(p: f64) -> Style {
    let t = theme();
    if p >= 100.0 {
        Style::default().fg(t.red).add_modifier(Modifier::BOLD)
    } else if p >= 80.0 {
        Style::default().fg(t.yellow)
    } else {
        Style::default().fg(t.green)
    }
}

/// `━━━━━━━━──────` at `width` cells.
fn bar(p: f64, width: usize) -> Vec<Span<'static>> {
    let t = theme();
    let filled = ((p.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    vec![
        Span::styled("━".repeat(filled), share_style(p)),
        Span::styled("─".repeat(width - filled), Style::default().fg(t.rule)),
    ]
}

fn limit_line(l: &Limit, label_w: usize, bar_w: usize) -> Line<'static> {
    let t = theme();
    let mut spans = vec![Span::styled(
        format!("  {:<label_w$} ", truncate(&l.label, label_w)),
        t.text(),
    )];
    match l.percent() {
        Some(p) => {
            spans.extend(bar(p, bar_w));
            spans.push(Span::styled(format!(" {p:>4.0}%"), share_style(p)));
            if p >= 100.0 {
                spans.push(Span::styled(" over", share_style(p)));
            }
        }
        None => spans.push(Span::styled(
            format!("{:<w$}", "no share reported", w = bar_w + 6),
            t.faint(),
        )),
    }
    let mut tail = vec![];
    if let Some(a) = l.amounts() {
        tail.push(a);
    }
    if let Some(r) = &l.resets_at {
        tail.push(format!("resets {}", fmt_reset(r)));
    }
    if !tail.is_empty() {
        spans.push(Span::styled(format!("  {}", tail.join(" · ")), t.muted()));
    }
    Line::from(spans)
}

impl UsageView {
    pub fn wheel(&mut self, delta: isize) {
        self.set_offset((self.top as isize + delta).max(0) as usize);
    }
    pub fn set_offset(&mut self, top: usize) {
        self.top = top.min(self.len.saturating_sub(self.view));
    }
    /// A click on visible row `i`: fold or unfold that provider.
    pub fn click(&mut self, i: usize) {
        if let Some((_, p)) = self.headers.iter().find(|(l, _)| *l == i) {
            let p = p.clone();
            if !self.folded.remove(&p) {
                self.folded.insert(p);
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits, data: UsageData) {
        let t = theme();
        let width = screen.width.saturating_sub(4).min(100);
        let w = width.saturating_sub(5) as usize;
        let label_w = 18.min(w / 4).max(8);
        let bar_w = w.saturating_sub(label_w + 40).clamp(8, 24);
        let mut lines: Vec<Line<'static>> = vec![];
        self.headers.clear();
        let mut providers = data.providers;
        // The active provider first, the rest as configured.
        providers.sort_by_key(|p| Some(&p.provider) != data.active.as_ref());
        if providers.is_empty() {
            lines.push(Line::from(Span::styled(
                "no providers configured",
                t.faint(),
            )));
        }
        let mut any_empty = false;
        let mut prev_empty = false;
        for (n, p) in providers.iter().enumerate() {
            let bars = p.bars();
            // Nothing to show: one line, the reason inline.
            let empty =
                bars.is_empty() && p.credits.is_none() && p.error.is_none() && p.notes.is_none();
            // One-line providers sit together; detailed ones get air.
            if n > 0 && !(empty && prev_empty) {
                lines.push(Line::default());
            }
            prev_empty = empty;
            let folded = self.folded.contains(&p.provider) || empty;
            if !empty {
                self.headers.push((lines.len(), p.provider.clone()));
            }
            let mut head = vec![
                Span::styled(if folded { "▸ " } else { "▾ " }, t.faint()),
                Span::styled(
                    p.label.clone(),
                    Style::default().fg(t.heading).add_modifier(Modifier::BOLD),
                ),
            ];
            if p.label != p.provider {
                head.push(Span::styled(format!(" {}", p.provider), t.faint()));
            }
            if Some(&p.provider) == data.active.as_ref() {
                head.push(Span::styled(" · active", Style::default().fg(t.accent)));
            }
            head.push(Span::styled(
                if p.subscription {
                    " · subscription"
                } else {
                    " · metered"
                },
                t.muted(),
            ));
            if empty {
                head[0] = Span::styled("  ", t.faint());
                head.push(Span::styled(
                    if p.declared {
                        " · no limits reported"
                    } else if p.subscription {
                        " · no limits reported · $0 extra per request"
                    } else {
                        " · no usage source"
                    },
                    t.faint(),
                ));
                any_empty |= !p.declared && !p.subscription;
            }
            if let Some(a) = &p.account {
                head.push(Span::styled(format!(" · {a}"), t.muted()));
            }
            if folded && let Some(l) = p.tightest() {
                let pct = l.percent().unwrap_or_default();
                head.push(Span::styled(format!("  {pct:.0}%"), share_style(pct)));
            }
            lines.push(Line::from(head));
            if folded {
                continue;
            }
            for l in &bars {
                lines.push(limit_line(l, label_w, bar_w));
            }
            if let Some(c) = &p.credits {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:<label_w$} ", "credits"), t.text()),
                    Span::styled(c.clone(), t.muted()),
                ]));
            }
            if let Some(notes) = &p.notes {
                lines.push(Line::from(Span::styled(format!("  {notes}"), t.faint())));
            }
            if let Some(e) = &p.error {
                lines.push(Line::from(Span::styled(
                    format!("  ⚠ {e}"),
                    Style::default().fg(t.yellow),
                )));
            }
            for w in &p.warnings {
                lines.push(Line::from(Span::styled(
                    format!("  ⚠ {w}"),
                    Style::default().fg(t.yellow),
                )));
            }
            if p.unknown_cost_calls > 0 && p.budget.is_some() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  + {} call{} of unknown cost this period (not counted)",
                        p.unknown_cost_calls,
                        if p.unknown_cost_calls == 1 { "" } else { "s" }
                    ),
                    t.faint(),
                )));
            }
            let mut state = vec![];
            if let Some(f) = &p.fetched {
                state.push(format!("read {}", f.format("%H:%M")));
            }
            if p.refreshing {
                state.push("refreshing…".into());
            } else if p.stale {
                state.push("stale · r refreshes".into());
            }
            if !state.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", state.join(" · ")),
                    t.faint(),
                )));
            }
        }
        if any_empty {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Add `usage: {argv, ttlSecs}` or `budget: {usd, period}` to a provider file to track it.",
                t.faint(),
            )));
        }
        // Truncate wide lines to the dialog (one column is the scrollbar).
        let lines: Vec<Line<'static>> = lines
            .into_iter()
            .map(|l| {
                let s: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                if s.width() > w {
                    let mut out = vec![];
                    let mut left = w;
                    for sp in l.spans {
                        if left == 0 {
                            break;
                        }
                        let c = truncate(&sp.content, left);
                        left = left.saturating_sub(c.width());
                        out.push(Span::styled(c, sp.style));
                    }
                    Line::from(out)
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
        while shown.len() < self.view {
            shown.push(Line::default());
        }
        shown.push(Line::default());
        shown.push(Line::from(Span::styled(
            if data.refreshing {
                "refreshing… · click a provider to fold · ↑↓ scroll · esc or q close"
            } else {
                "r refresh · click a provider to fold · ↑↓ scroll · esc or q close"
            },
            t.faint(),
        )));
        let title = if data.refreshing {
            " Usage · refreshing… "
        } else {
            " Usage "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .title(Span::styled(title, t.bold()))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(t.surface));
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        frame.render_widget(Paragraph::new(shown).block(block), area);
        for (line, _) in &self.headers {
            if *line >= self.top && *line < self.top + self.view {
                hits.add(
                    Rect {
                        x: area.x + 1,
                        y: area.y + 1 + (*line - self.top) as u16,
                        width: area.width.saturating_sub(3),
                        height: 1,
                    },
                    Target::Row(*line),
                );
            }
        }
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
