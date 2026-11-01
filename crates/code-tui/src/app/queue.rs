//! The queue box (D73, D74): messages typed while the viewed agent works,
//! each with three class buttons (next turn, steer, interrupt = send now;
//! the selected one coloured), edit and cancel (adapted from
//! FlyCockpit's Cockpit). The engine adapter owns the queue; this module only shows and edits it.
use super::*;
use crate::engine::{QueueClass, QueueOutcome, Queued};
use crate::ui::QAct;
use unicode_width::UnicodeWidthStr;

/// Queue rows the box shows before it scrolls.
pub(super) const QUEUE_ROWS: usize = 4;

pub(super) fn class_color(class: QueueClass) -> Color {
    let t = theme();
    match class {
        QueueClass::NextTurn => t.fog,
        QueueClass::Steer => t.yellow,
        QueueClass::Interrupt => t.red,
    }
}

impl QAct {
    fn label(self) -> &'static str {
        match self {
            QAct::Set(QueueClass::NextTurn) => "[next turn]",
            QAct::Set(QueueClass::Steer) => "[steer]",
            QAct::Set(QueueClass::Interrupt) => "[interrupt]",
            QAct::SendNow => "[send now]",
            QAct::Edit => "[edit]",
            QAct::Cancel => "[x]",
            QAct::Toggle => "",
        }
    }
}

impl App {
    /// The agent the composer and the queue box address.
    pub(super) fn queue_agent(&self) -> String {
        self.view.clone().unwrap_or_else(|| "root".into())
    }

    pub(super) fn queue_items(&self) -> Vec<Queued> {
        self.engine.queued(&self.queue_agent())
    }

    /// Rows the box takes above the rule (its top border plus its rows).
    pub(super) fn queue_height(&self) -> u16 {
        let n = self.queue_items().len();
        if n == 0 {
            0
        } else {
            1 + n.min(QUEUE_ROWS) as u16
        }
    }

    /// The engine's queue changed: keep focus and scroll valid.
    pub(super) fn queue_changed(&mut self) {
        let items = self.queue_items();
        if let Some(id) = self.queue_focus
            && !items.iter().any(|q| q.id == id)
        {
            self.queue_focus = None;
        }
        let max = items.len().saturating_sub(QUEUE_ROWS);
        self.queue_scroll = self.queue_scroll.min(max);
        self.dirty = true;
    }

    /// Queue `text` for the viewed agent with its default class (or the
    /// class of the message being edited).
    pub(super) fn enqueue(&mut self, text: String) {
        let agent = self.queue_agent();
        if self
            .queue_edit
            .as_ref()
            .is_some_and(|(q, _)| q.agent == agent)
            && let Some((mut item, at)) = self.queue_edit.take()
        {
            item.text = text;
            let class = item.class;
            self.engine.requeue(item, at);
            self.queue_changed();
            self.queue_scroll = at.saturating_sub(QUEUE_ROWS - 1);
            let effect = self.engine.queue_effect(&agent, class);
            return self.hint(format!("{} · {effect}", class.label()));
        }
        let class = self.catalog.queue_default;
        match self.engine.queue_message(&agent, text.clone(), class) {
            Ok(QueueOutcome::Held(_)) => {
                // The box follows its newest message.
                self.queue_scroll = usize::MAX;
                let effect = self.engine.queue_effect(&agent, class);
                self.hint(format!(
                    "queued · {} · {effect} · enter again escalates",
                    class.label()
                ));
            }
            Ok(QueueOutcome::Delivered) => {
                let label = self.tree.label(&agent);
                self.hint(format!("sent to {label} · reaches it at its next boundary"));
            }
            Err(e) => {
                self.composer.set_wire(&text);
                self.composer.edited();
                self.hint(e);
            }
        }
        self.queue_changed();
    }

    /// Enter on an empty composer: escalate the most recent queued message
    /// (next turn → steer → interrupt). False when nothing is queued.
    pub(super) fn escalate_last(&mut self) -> bool {
        let Some(last) = self.queue_items().pop() else {
            return false;
        };
        match last.class.escalated() {
            Some(class) => self.set_class(last.id, class),
            None => self.hint(format!(
                "already interrupt · {}",
                self.engine.queue_effect(&last.agent, QueueClass::Interrupt)
            )),
        }
        true
    }

    fn set_class(&mut self, id: u64, class: QueueClass) {
        let agent = self.queue_agent();
        match self.engine.reclassify(id, class) {
            Ok(()) => {
                let effect = self.engine.queue_effect(&agent, class);
                self.hint(format!("{} · {effect}", class.label()));
            }
            Err(e) => self.hint(e),
        }
        self.queue_changed();
    }

    pub(super) fn queue_act(&mut self, id: u64, act: QAct) {
        let Some(item) = self.queue_items().into_iter().find(|q| q.id == id) else {
            return self.queue_changed();
        };
        match act {
            QAct::Set(class) if class == item.class => {
                let effect = self.engine.queue_effect(&item.agent, class);
                self.hint(format!("{} · {effect}", class.label()));
            }
            QAct::Set(class) => self.set_class(id, class),
            QAct::SendNow => self.set_class(id, QueueClass::Interrupt),
            QAct::Toggle => self.set_class(id, item.class.toggled()),
            QAct::Edit => self.edit_queued(id),
            QAct::Cancel => {
                self.engine.unqueue(id);
                self.queue_changed();
                self.hint("queued message removed");
            }
        }
    }

    /// Move a queued message back into the composer; enter re-queues it
    /// in its place with its class.
    fn edit_queued(&mut self, id: u64) {
        if !self.composer.is_empty() {
            return self.hint("send or clear the draft before editing a queued message");
        }
        if let Some((item, at)) = self.engine.unqueue(id) {
            self.composer.set_wire(&item.text);
            self.composer.edited();
            self.queue_edit = Some((item, at));
            self.queue_focus = None;
            self.hint("editing a queued message · enter puts it back");
        }
        self.queue_changed();
    }

    /// Leave a queued-message edit: the message goes back in its place,
    /// unchanged (`edited` false: ctrl+c cleared the composer) or with the
    /// composer's text (the view changed), and the composer empties.
    pub(super) fn restore_queue_edit(&mut self, edited: bool) {
        if let Some((mut item, at)) = self.queue_edit.take() {
            if edited && !self.composer.is_empty() {
                item.text = self.composer.wire();
            }
            self.composer.buf.clear();
            self.composer.edited();
            self.engine.requeue(item, at);
            self.queue_changed();
            self.hint("the queued message went back in the queue");
        }
    }

    /// Ctrl+↑ from the composer: focus the newest queued message.
    pub(super) fn focus_queue(&mut self) -> bool {
        let Some(last) = self.queue_items().pop() else {
            return false;
        };
        self.queue_focus = Some(last.id);
        self.queue_reveal();
        true
    }

    fn queue_reveal(&mut self) {
        let items = self.queue_items();
        if let Some(i) = self
            .queue_focus
            .and_then(|id| items.iter().position(|q| q.id == id))
        {
            if i < self.queue_scroll {
                self.queue_scroll = i;
            } else if i >= self.queue_scroll + QUEUE_ROWS {
                self.queue_scroll = i + 1 - QUEUE_ROWS;
            }
        }
    }

    /// Keys while a queued message has focus. True when consumed.
    pub(super) fn queue_key(&mut self, key: KeyEvent) -> bool {
        let Some(id) = self.queue_focus else {
            return false;
        };
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        let items = self.queue_items();
        let Some(i) = items.iter().position(|q| q.id == id) else {
            self.queue_focus = None;
            return false;
        };
        match key.code {
            KeyCode::Esc => self.queue_focus = None,
            KeyCode::Up => self.queue_focus = Some(items[i.saturating_sub(1)].id),
            KeyCode::Down => self.queue_focus = items.get(i + 1).map(|q| q.id),
            KeyCode::Char('s') | KeyCode::Char('S') => self.queue_act(id, QAct::SendNow),
            KeyCode::Char('t') | KeyCode::Char('T') => self.queue_act(id, QAct::Toggle),
            KeyCode::Char('e') | KeyCode::Char('E') | KeyCode::Enter => {
                self.queue_act(id, QAct::Edit)
            }
            KeyCode::Char('x') | KeyCode::Char('X') | KeyCode::Delete | KeyCode::Backspace => {
                self.queue_act(id, QAct::Cancel);
                // Focus stays in the box on the next message.
                let rest = self.queue_items();
                self.queue_focus = rest.get(i.min(rest.len().saturating_sub(1))).map(|q| q.id);
            }
            _ => {}
        }
        self.queue_reveal();
        true
    }

    pub(super) fn queue_wheel(&mut self, delta: isize) {
        let max = self.queue_items().len().saturating_sub(QUEUE_ROWS);
        self.queue_scroll =
            (self.queue_scroll as isize + delta.signum()).clamp(0, max as isize) as usize;
    }

    /// The box: a top border with the summary, then one row per message.
    /// Its bottom border is the rule below (drawn with corners by the caller).
    pub(super) fn draw_queue(&mut self, frame: &mut Frame, area: Rect) {
        self.queue_area = area;
        let items = self.queue_items();
        if items.is_empty() || area.height < 2 || area.width < 20 {
            return;
        }
        let t = theme();
        let width = area.width as usize;
        let border = Style::default().fg(t.rule);
        // Summary: the most urgent class, the count, and what it does now.
        let peak = items
            .iter()
            .map(|q| q.class)
            .max_by_key(|c| match c {
                QueueClass::NextTurn => 0,
                QueueClass::Steer => 1,
                QueueClass::Interrupt => 2,
            })
            .unwrap_or_default();
        let agent = self.queue_agent();
        let n = items.len();
        let summary = format!(
            " · {n} queued · {} ",
            self.engine.queue_effect(&agent, peak)
        );
        let keys = if self.queue_focus.is_some() {
            " esc leave "
        } else {
            " ctrl+↑ select "
        };
        let peak_label = format!(" {}", peak.label());
        let fixed = 2 + peak_label.width() + keys.width() + 2;
        let summary = truncate(&summary, width.saturating_sub(fixed));
        let fill = width.saturating_sub(fixed + summary.width());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("╭─", border),
                Span::styled(
                    peak_label,
                    Style::default()
                        .fg(class_color(peak))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(summary, t.muted()),
                Span::styled("─".repeat(fill), border),
                Span::styled(keys, t.faint()),
                Span::styled("─╮", border),
            ])),
            Rect { height: 1, ..area },
        );
        let rows = (area.height - 1) as usize;
        let max = n.saturating_sub(rows);
        self.queue_scroll = self.queue_scroll.min(max);
        let acts = [
            QAct::Set(QueueClass::NextTurn),
            QAct::Set(QueueClass::Steer),
            QAct::Set(QueueClass::Interrupt),
            QAct::Edit,
            QAct::Cancel,
        ];
        let acts_w: usize = acts.iter().map(|a| a.label().width() + 1).sum();
        for (r, q) in items.iter().skip(self.queue_scroll).take(rows).enumerate() {
            let y = area.y + 1 + r as u16;
            let focused = self.queue_focus == Some(q.id);
            let row = Rect {
                x: area.x + 1,
                y,
                width: area.width.saturating_sub(2),
                height: 1,
            };
            // Side borders (the right one doubles as the scrollbar track).
            for x in [area.x, area.right() - 1] {
                if let Some(c) = frame.buffer_mut().cell_mut((x, y)) {
                    c.set_symbol("│");
                    c.set_style(border);
                }
            }
            let inner = row.width as usize;
            let text_w = inner.saturating_sub(1 + acts_w + 1);
            let text = crate::paste::collapse(&q.text).replace('\n', " ⏎ ");
            let text = truncate(text.trim(), text_w);
            let pad = text_w.saturating_sub(text.width());
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(text, Style::default().fg(t.fog)),
                Span::raw(" ".repeat(pad + 1)),
            ];
            // The selected class is coloured and bold; the others are dim.
            for a in acts {
                let style = match a {
                    QAct::Set(c) if c == q.class => Style::default()
                        .fg(class_color(c))
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default().fg(t.dim),
                };
                spans.push(Span::styled(a.label(), style));
                spans.push(Span::raw(" "));
            }
            let mut line = Line::from(spans);
            if focused {
                for s in &mut line.spans {
                    s.style = s.style.bg(t.select_bg);
                    if s.style.fg == Some(t.fog) {
                        s.style = s.style.fg(t.ink);
                    }
                }
            }
            frame.render_widget(Paragraph::new(line), row);
            // The row (click focuses it), then its chips on top.
            self.hits.add(row, Target::Queue(q.id));
            let mut x = row.right().saturating_sub(acts_w as u16);
            for a in acts {
                let w = a.label().width() as u16;
                self.hits.add(
                    Rect {
                        x,
                        y,
                        width: w,
                        height: 1,
                    },
                    Target::QueueAct(q.id, a),
                );
                x += w + 1;
            }
        }
        ui::scrollbar(
            frame.buffer_mut(),
            &mut self.hits,
            ScrollId::Queue,
            Rect {
                y: area.y + 1,
                height: rows as u16,
                ..area
            },
            n,
            rows,
            self.queue_scroll,
        );
    }
}
