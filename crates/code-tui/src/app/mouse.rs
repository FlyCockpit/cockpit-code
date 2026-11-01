//! Mouse and pointer handling: hit dispatch against the rects recorded at
//! render time, hover tracking, scrollbars (track paging, thumb drags),
//! inner box wheel routing, drag selection with edge auto-scroll, copy
//! buttons and the right-click menu, keyboard focus on transcript entries.
use super::view::{Menu, Sel};
use super::*;
use crate::clipboard::{Delivery, Outcome as ClipOutcome, TMUX_HINT};
use crate::markdown;
use crate::rows::{self, Btn};
use crate::ui::ScrollGeom;

/// A left press, until release.
#[derive(Clone)]
pub(super) struct Press {
    pub x: u16,
    pub y: u16,
    pub hit: Option<Target>,
    /// Transcript position (absolute row, column) when it started there.
    pub text: Option<(usize, u16)>,
    /// The press started inside this entry's inner box.
    pub boxed: Option<usize>,
    pub dragged: bool,
}

/// Keyboard focus on a transcript entry (copy buttons, box scrolling).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Focus {
    pub entry: usize,
    pub button: Option<Btn>,
}

fn in_rect(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
}

impl App {
    pub(super) fn on_mouse(&mut self, m: MouseEvent) {
        self.mouse_pos = Some((m.column, m.row));
        if self.term_mouse(m) {
            return;
        }
        match m.kind {
            MouseEventKind::Moved => {
                // Re-render only when the hovered element changes.
                let hit = self.hits.at(m.column, m.row);
                let entry = self.layout.entry_near(m.column, m.row);
                self.dirty = hit != self.hover || entry != self.hover_entry;
                return;
            }
            MouseEventKind::Drag(MouseButton::Left) => return self.on_drag(m.column, m.row),
            MouseEventKind::Up(MouseButton::Left) => return self.on_release(m.column, m.row),
            _ => {}
        }
        if !matches!(self.overlay, Overlay::None) {
            return self.overlay_mouse(m);
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.wheel(m.column, m.row, -3),
            MouseEventKind::ScrollDown => self.wheel(m.column, m.row, 3),
            MouseEventKind::Down(MouseButton::Left) => self.on_press(m.column, m.row),
            MouseEventKind::Down(MouseButton::Right) => self.context_menu(m.column, m.row),
            _ => {}
        }
    }

    fn overlay_mouse(&mut self, m: MouseEvent) {
        let delta = match m.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => 0,
        };
        if delta != 0 {
            return self.overlay_wheel(m.column, m.row, delta);
        }
        if m.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let Some((_, target)) = self.hits.at(m.column, m.row) else {
            // A click outside a context menu closes it.
            if matches!(self.overlay, Overlay::Menu(_)) {
                self.overlay = Overlay::None;
            }
            return;
        };
        match target {
            Target::Scroll(id) => self.scroll_press(id, m.row),
            Target::Tab(i) => {
                if let Overlay::Questions(d) = &mut self.overlay {
                    d.click_tab(i);
                }
            }
            Target::Act(a) => match &mut self.overlay {
                Overlay::Questions(d) => {
                    let outcome = d.act(a);
                    self.question_outcome(outcome);
                }
                Overlay::Agents(v) => {
                    let action = v.click_act(a);
                    self.agents_action(action);
                }
                Overlay::Decisions(v) => {
                    let action = v.click_act(a);
                    self.decisions_action(action);
                }
                _ => {}
            },
            Target::Open(path, line) => self.open_in_pager(std::path::Path::new(&path), line),
            Target::Row(i) => self.overlay_click(i),
            Target::Cell(i, cell) => {
                if let Overlay::Picker(p) = &mut self.overlay {
                    let action = p.click_cell(i, cell);
                    self.picker_action(action);
                }
            }
            _ => {}
        }
    }

    /// D130: the wheel scrolls the view under the pointer, never a
    /// selection. Inside the overlay: the pane under the pointer (a
    /// recorded region, else the overlay's main view). Outside it: what is
    /// under the pointer there (the transcript, strip, composer …); a
    /// context menu closes first, since it belongs to the spot it opened at.
    fn overlay_wheel(&mut self, x: u16, y: u16, delta: isize) {
        let inside = self.hits.modal.is_none_or(|r| in_rect(r, x, y));
        if !inside {
            if matches!(self.overlay, Overlay::Menu(_)) {
                self.overlay = Overlay::None;
            }
            return self.wheel(x, y, delta);
        }
        if self.wheel_region(x, y, delta) {
            return;
        }
        match &mut self.overlay {
            Overlay::Picker(p) => p.wheel(delta),
            Overlay::List(l, _) => l.wheel(delta),
            Overlay::Pager(p) => p.wheel(delta),
            Overlay::Dialog(d, _) => d.wheel(delta),
            Overlay::Cost(c) => c.wheel(delta),
            Overlay::Usage(u) => u.wheel(delta),
            Overlay::Questions(d) => d.wheel(delta),
            Overlay::ForkChat(c) => c.wheel(delta),
            Overlay::Criteria(v) => v.wheel(delta),
            Overlay::Contract(ed, _) => ed.wheel(delta),
            Overlay::Diff(d) => d.scroll(delta),
            Overlay::Agents(v) => v.wheel(delta),
            Overlay::Decisions(v) => v.wheel(delta),
            // Menus are short and never scroll; the wheel leaves the
            // highlighted item alone.
            Overlay::Menu(_) | Overlay::None => {}
        }
    }

    /// The wheel over a recorded scroll region (D130). True when one is
    /// under the pointer (even at its end, so nothing behind it scrolls).
    fn wheel_region(&mut self, x: u16, y: u16, delta: isize) -> bool {
        let Some((id, g)) = self.hits.region_at(x, y) else {
            return false;
        };
        let off = (g.offset as isize + delta).clamp(0, g.max() as isize) as usize;
        self.set_scroll(&id, off, &g);
        true
    }

    /// A click on row `i` of the open overlay: the same as selecting it and
    /// pressing Enter.
    fn overlay_click(&mut self, i: usize) {
        match &mut self.overlay {
            Overlay::Picker(p) => {
                let action = p.click(i);
                self.picker_action(action);
            }
            Overlay::List(l, _) => {
                let action = l.click(i);
                let selected = l.selected_id();
                self.list_action(action, selected);
            }
            Overlay::Dialog(d, _) => {
                let outcome = d.click(i);
                self.dialog_outcome(outcome);
            }
            Overlay::Questions(d) => {
                let outcome = d.click_row(i);
                self.question_outcome(outcome);
            }
            Overlay::Menu(menu) => {
                let (entry, btn) = (menu.entry, menu.items.get(i).copied());
                self.overlay = Overlay::None;
                if let Some(b) = btn {
                    self.run_button(entry, b);
                }
            }
            Overlay::Usage(u) => u.click(i),
            Overlay::Agents(v) => {
                let action = v.click(i);
                self.agents_action(action);
            }
            Overlay::Decisions(v) => v.click(i),
            Overlay::Criteria(v) => v.click(i),
            Overlay::Contract(ed, _) => ed.click(i),
            Overlay::Diff(d) => d.click(i),
            Overlay::Pager(_) | Overlay::Cost(_) | Overlay::ForkChat(_) | Overlay::None => {}
        }
    }

    fn wheel(&mut self, x: u16, y: u16, delta: isize) {
        // Popups above the composer, the composer itself, panes.
        if self.wheel_region(x, y, delta) {
            return;
        }
        if in_rect(self.comp_area, x, y) {
            // A composer that fits has nothing to scroll; the transcript
            // is not under the pointer, so nothing moves.
            return;
        }
        if in_rect(self.queue_area, x, y) {
            return self.queue_wheel(delta);
        }
        if in_rect(self.strip_area, x, y) {
            let (_, items) = self.strip_items();
            let max = items.len().saturating_sub(view::STRIP_ROWS);
            self.strip_scroll =
                (self.strip_scroll as isize + delta.signum()).clamp(0, max as isize) as usize;
            return;
        }
        if in_rect(self.goals_area, x, y) {
            self.goals_scroll = (self.goals_scroll as isize + delta).max(0) as usize;
            return;
        }
        let a = self.layout.area;
        if y >= a.y && y < a.bottom() && x < self.transcript_area.right() {
            let abs = self.layout.abs_at(y);
            if let Some((i, j)) = self.layout.locate(abs) {
                let width = a.width as usize;
                let tick = self.tick;
                let tr = self.view_transcript();
                if let Some(e) = tr.entries.get_mut(i)
                    && e.render(width, tick).meta.get(j).is_some_and(|m| m.boxed)
                    && e.scroll_box(delta)
                {
                    return;
                }
            }
        }
        self.scroll_by(delta);
    }

    fn on_press(&mut self, x: u16, y: u16) {
        let hit = self.hits.at(x, y).map(|(_, t)| t);
        if let Some(Target::Scroll(id)) = &hit {
            return self.scroll_press(id.clone(), y);
        }
        let a = self.layout.area;
        let mut text = None;
        let mut boxed = None;
        if y >= a.y && y < a.bottom() && x >= a.x && x < a.right() {
            let abs = self.layout.abs_at(y);
            text = Some((abs, x - a.x));
            if let Some((i, j)) = self.layout.locate(abs) {
                let width = a.width as usize;
                let tick = self.tick;
                let tr = self.view_transcript();
                if tr.entries[i].render(width, tick).meta[j].boxed {
                    boxed = Some(i);
                }
            }
        }
        self.press = Some(Press {
            x,
            y,
            hit,
            text,
            boxed,
            dragged: false,
        });
    }

    fn on_drag(&mut self, x: u16, y: u16) {
        if let Some((id, grab)) = self.thumb_drag.clone() {
            if let Some(g) = self.hits.scroll(&id) {
                let off = g.offset_at(y as isize - g.track.y as isize - grab);
                self.set_scroll(&id, off, &g);
            }
            return;
        }
        let Some(p) = &mut self.press else { return };
        if !p.dragged && (p.x, p.y) == (x, y) {
            return;
        }
        p.dragged = true;
        let Some(start) = p.text else { return };
        let boxed = p.boxed;
        let focus = self.drag_pos(x, y);
        self.sel = Some(Sel {
            anchor: start,
            focus,
            active: true,
            boxed,
        });
    }

    /// Transcript position under the pointer, clamped into the view.
    fn drag_pos(&self, x: u16, y: u16) -> (usize, u16) {
        let a = self.layout.area;
        let abs = self.layout.abs_at(y);
        let col = if y < a.y {
            0
        } else if y >= a.bottom() {
            a.width
        } else {
            x.saturating_sub(a.x).min(a.width)
        };
        (abs, col)
    }

    /// Called every tick: auto-scroll while a drag passes the pane edge.
    pub fn on_tick(&mut self) -> bool {
        let mut changed = self.poll_background();
        changed |= self.term_tick();
        changed |= self.git.poll();
        changed |= self.mention_poll();
        if let (Some(p), Some((x, y))) = (&self.press, self.mouse_pos)
            && p.dragged
            && self.sel.is_some_and(|s| s.active)
        {
            let a = self.layout.area;
            let dir = if y < a.y {
                -1
            } else if y >= a.bottom() {
                1
            } else {
                0
            };
            if dir != 0 {
                let boxed = self.sel.and_then(|s| s.boxed);
                match boxed {
                    Some(e) => {
                        let tr = self.view_transcript();
                        if let Some(entry) = tr.entries.get_mut(e) {
                            entry.scroll_box(dir);
                        }
                    }
                    None => self.scroll_by(dir),
                }
                let pos = self.drag_pos(x, y);
                if let Some(s) = &mut self.sel {
                    s.focus = pos;
                }
                changed = true;
            }
        }
        if self.transcript.prune_abandoned() {
            changed = true;
        }
        // Startup MCP health check (D69): report each broken server once.
        if let Some(until) = self.mcp_check_until
            && self.tick.is_multiple_of(5)
        {
            for (name, why) in self.engine.mcp_health() {
                if !self.mcp_problems.contains_key(&name) {
                    self.transcript.notice(
                        Level::Warn,
                        format!("MCP server {name} is not working: {why} · /mcp shows its state"),
                    );
                    self.mcp_problems.insert(name, why);
                    changed = true;
                }
            }
            if Instant::now() >= until {
                self.mcp_check_until = None;
            }
        }
        // The MCP picker shows live connection state.
        if self.tick.is_multiple_of(5) && matches!(self.overlay, Overlay::List(_, ListPurpose::Mcp))
        {
            let rows = self.mcp_rows();
            if let Overlay::List(l, _) = &mut self.overlay {
                let fresh: Vec<_> = rows.iter().map(|r| (&r.label, &r.detail)).collect();
                let old: Vec<_> = l.rows.iter().map(|r| (&r.label, &r.detail)).collect();
                if fresh != old {
                    l.rows = rows;
                    changed = true;
                }
            }
        }
        for a in &mut self.tree.agents {
            if a.transcript.prune_abandoned() {
                changed = true;
            }
        }
        changed
    }

    fn on_release(&mut self, x: u16, y: u16) {
        if self.thumb_drag.take().is_some() {
            return;
        }
        let Some(p) = self.press.take() else { return };
        if p.dragged {
            if let Some(s) = &mut self.sel
                && s.active
            {
                s.active = false;
                self.copy_selection();
            }
            return;
        }
        // A click: the target under both press and release.
        let now = self.hits.at(x, y).map(|(_, t)| t);
        if now != p.hit {
            return;
        }
        self.sel = None;
        if let Some(target) = now {
            self.click(target, y);
        } else if self.focus.is_some() {
            self.focus = None;
        }
    }

    fn click(&mut self, target: Target, y: u16) {
        match target {
            Target::Button(e, btn) => {
                self.focus = Some(Focus {
                    entry: e,
                    button: Some(btn),
                });
                self.run_button(e, btn);
            }
            Target::Header(e, line) => self.toggle_entry(e, line, y),
            Target::Task(id) => self.open_view(&id),
            Target::Url(url) => match crate::links::open(&url) {
                Ok(()) => self.hint(format!("opened {}", truncate(&url, 60))),
                Err(e) => self.hint(e),
            },
            Target::User(e) => {
                let tr = self.view_transcript();
                let on = tr.anchor() != Some(e);
                tr.set_anchor(on.then_some(e));
                self.focus = on.then_some(Focus {
                    entry: e,
                    button: Some(Btn::Fork),
                });
            }
            Target::Sticky(e) => {
                self.anchor_req = Some((e, 0, 0));
            }
            Target::Status(seg) => match seg {
                StatusSeg::Title => {
                    let title = self.title.clone().unwrap_or_default();
                    self.composer.buf.set(format!("/rename {title}"));
                    self.hint("edit the title and press enter");
                }
                StatusSeg::Model => self.command("model", ""),
                StatusSeg::Effort => self.command("effort", ""),
                StatusSeg::Mode => self.command("permissions", ""),
                StatusSeg::Cost => self.command("cost", ""),
                StatusSeg::Quota => self.command("usage", ""),
                StatusSeg::Goals => {
                    if self.goal.is_some() {
                        self.open_goal_status()
                    } else {
                        self.open_criteria()
                    }
                }
                StatusSeg::Agents => self.open_agents(None),
                StatusSeg::Decisions => self.open_decisions(),
                StatusSeg::Jobs => self.open_jobs(None),
                StatusSeg::Git => self.command("diff", ""),
                StatusSeg::Pending => self.command("answer", ""),
                StatusSeg::Terminals => self.term_toggle(),
            },
            Target::Agent(id) => {
                self.strip_focus = false;
                self.open_view(&id);
            }
            Target::Finished => self.show_finished = true,
            Target::Slash(i) => {
                let matches = self.slash_matches();
                if let Some((cmd, _)) = matches.get(i) {
                    let cmd = cmd.clone();
                    self.composer.buf.set(format!("/{cmd}"));
                    self.submit();
                }
            }
            Target::Mention(i) => self.mention_accept(i),
            Target::Chip(b) => self.chip_menu(b, y),
            Target::Queue(id) => {
                self.queue_focus = (self.queue_focus != Some(id)).then_some(id);
            }
            Target::QueueAct(id, act) => self.queue_act(id, act),
            Target::Open(path, line) => self.open_in_pager(std::path::Path::new(&path), line),
            Target::StripAct(id, act) => self.strip_act(&id, act),
            Target::Scroll(_)
            | Target::Term(_)
            | Target::Row(_)
            | Target::Cell(..)
            | Target::Tab(_)
            | Target::Act(_) => {}
        }
    }

    /// Expand/collapse entry `e` (clicked on its row `line` at screen row
    /// `y`), keeping the header on the same screen row.
    pub(super) fn toggle_entry(&mut self, e: usize, line: usize, y: u16) {
        let off = y as isize - self.layout.area.y as isize - line as isize;
        let tr = self.view_transcript();
        let Some(entry) = tr.entries.get_mut(e) else {
            return;
        };
        if !entry.selectable() {
            return;
        }
        entry.expanded = !entry.expanded;
        self.anchor_req = Some((e, 0, off));
        self.sel = None;
    }

    fn scroll_press(&mut self, id: ScrollId, y: u16) {
        let Some(g) = self.hits.scroll(&id) else {
            return;
        };
        let (pos, len) = g.thumb();
        let rel = y as isize - g.track.y as isize;
        if rel >= pos as isize && rel < (pos + len) as isize {
            self.thumb_drag = Some((id, rel - pos as isize));
        } else {
            // Page toward the click.
            let page = g.view.saturating_sub(1).max(1);
            let off = if rel < pos as isize {
                g.offset.saturating_sub(page)
            } else {
                (g.offset + page).min(g.max())
            };
            self.set_scroll(&id, off, &g);
        }
    }

    fn set_scroll(&mut self, id: &ScrollId, off: usize, g: &ScrollGeom) {
        let off = off.min(g.max());
        match id {
            ScrollId::Transcript => {
                self.scroll = (off < g.max()).then_some(off);
            }
            ScrollId::Box(e) => {
                let tr = self.view_transcript();
                if let Some(entry) = tr.entries.get_mut(*e) {
                    let follow = entry.boxinfo().is_some_and(|b| b.follow);
                    entry.box_scroll = if follow && off >= g.max() {
                        None
                    } else {
                        Some(off)
                    };
                }
            }
            ScrollId::Strip => self.strip_scroll = off,
            ScrollId::Terminal => self.term_set_scroll(off),
            ScrollId::Goals => self.goals_scroll = off,
            ScrollId::Queue => self.queue_scroll = off,
            ScrollId::Slash => self.slash_top = Some(off),
            ScrollId::Mentions => self.mention_top = Some(off),
            ScrollId::OverlayList => match &mut self.overlay {
                Overlay::Diff(d) => d.set_list_offset(off),
                Overlay::Agents(v) => v.set_detail_offset(off),
                Overlay::Decisions(v) => v.set_detail_offset(off),
                _ => {}
            },
            ScrollId::Composer => {
                let delta = off as isize - self.comp_skip as isize;
                for _ in 0..delta.unsigned_abs() {
                    self.composer.buf.vertical(delta < 0);
                }
            }
            ScrollId::Overlay => match &mut self.overlay {
                Overlay::Picker(p) => p.set_offset(off),
                Overlay::List(l, _) => l.set_offset(off),
                Overlay::Pager(p) => p.set_offset(off),
                Overlay::Dialog(d, _) => d.set_offset(off),
                Overlay::Cost(c) => c.set_offset(off),
                Overlay::Usage(u) => u.set_offset(off),
                Overlay::Questions(d) => d.set_offset(off),
                Overlay::Criteria(v) => v.set_offset(off),
                Overlay::Contract(ed, _) => ed.set_offset(off),
                Overlay::Diff(d) => d.set_offset(off),
                Overlay::Agents(v) => v.set_offset(off),
                Overlay::Decisions(v) => v.set_offset(off),
                Overlay::Menu(_) | Overlay::ForkChat(_) | Overlay::None => {}
            },
        }
    }

    // ---- copy ---------------------------------------------------------

    /// The selection as plain text (rendered text without gutters, borders
    /// or wrap breaks).
    fn selection_text(&mut self) -> Option<String> {
        let sel = self.sel?;
        let ((r0, _), (r1, _)) = sel.ordered();
        let layout = self.layout.clone();
        let width = layout.area.width as usize;
        let tick = self.tick;
        let tr = self.view_transcript();
        let mut rows_: Vec<(
            ratatui::text::Line<'static>,
            crate::rows::Meta,
            usize,
            usize,
        )> = vec![];
        for abs in r0..=r1 {
            let Some((c0, c1)) = sel.cols(abs) else {
                continue;
            };
            match layout.locate(abs) {
                Some((i, j)) => {
                    let r = tr.entries[i].render(width, tick);
                    let meta = r.meta[j].clone();
                    if sel.boxed.is_some_and(|b| b != i || !meta.boxed) {
                        continue;
                    }
                    rows_.push((r.lines[j].clone(), meta, c0, c1));
                }
                None if sel.boxed.is_none() => {
                    rows_.push((Default::default(), Default::default(), 0, 0));
                }
                None => {}
            }
        }
        let text = rows::join(rows_.iter().map(|(l, m, a, b)| (l, m, *a, *b)));
        (!text.is_empty()).then_some(text)
    }

    pub(super) fn copy_selection(&mut self) {
        match self.selection_text() {
            Some(text) => {
                let d = self.clip.copy_plain(&text);
                self.report(d, &format!("{} chars", text.chars().count()), "");
            }
            None => self.hint("nothing selected · drag in the chat to select"),
        }
    }

    /// Toast the outcome of a copy.
    fn report(&mut self, d: Delivery, what: &str, downgrade: &str) {
        let what = if d.downgraded { "it" } else { what };
        let mut msg = match &d.outcome {
            ClipOutcome::Copied(route) => format!("copied {what} ({route})"),
            ClipOutcome::Sent(route) => {
                format!("sent {what} to the terminal ({route}; OSC 52 is unconfirmed)")
            }
            ClipOutcome::Failed(e) => format!("copy failed: {e}"),
        };
        if d.downgraded && !matches!(d.outcome, ClipOutcome::Failed(_)) {
            msg = format!("{downgrade} · {msg}");
        }
        self.hint(msg);
        // OSC 52 inside tmux needs tmux's cooperation: say how, once.
        if self.clip.tmux && !self.clip.tmux_hinted && matches!(d.outcome, ClipOutcome::Sent(_)) {
            self.clip.tmux_hinted = true;
            self.view_transcript().notice(Level::Info, TMUX_HINT);
        }
    }

    pub(super) fn run_button(&mut self, e: usize, btn: Btn) {
        // Composer paste chips (from the chip menu).
        match btn {
            Btn::ChipView(b) => {
                if let Some(blk) = self.composer.buf.blocks.get(b) {
                    let title = format!("paste #{} · read-only", blk.n);
                    let text = blk.full.clone();
                    self.overlay = Overlay::Pager(Box::new(Pager::text(title, &text)));
                }
                return;
            }
            Btn::ChipEdit(b) => return self.edit_chip(b),
            Btn::ChipExpand(b) => {
                self.composer.buf.expand_block(b);
                self.composer.edited();
                return;
            }
            Btn::ChipRemove(b) => {
                self.composer.buf.remove_block(b);
                self.composer.edited();
                return;
            }
            Btn::ChipOpen(b) => {
                if let Some(blk) = self.composer.buf.blocks.get(b) {
                    let path = std::path::PathBuf::from(&blk.full);
                    self.open_file(&path);
                }
                return;
            }
            Btn::Output => {
                let tr = self.view_transcript();
                let path = match tr.entries.get(e).map(|x| &x.kind) {
                    Some(Kind::Tool(t)) => t
                        .output
                        .as_deref()
                        .and_then(crate::transcript::full_output)
                        .map(|(_, _, p)| p),
                    _ => None,
                };
                if let Some(p) = path {
                    self.open_in_pager(&p, None);
                }
                return;
            }
            Btn::OpenFork => {
                let tr = self.view_transcript();
                if let Some(Kind::ForkAnswer { question, summary }) =
                    tr.entries.get(e).map(|x| &x.kind)
                {
                    let (q, sum) = (question.clone(), summary.clone());
                    self.reopen_fork(&q, &sum);
                }
                return;
            }
            _ => {}
        }
        let tr = self.view_transcript();
        let Some(entry) = tr.entries.get(e) else {
            return;
        };
        let (md, user, diff) = match &entry.kind {
            // The turn's copy row copies the whole turn.
            Kind::Assistant(_) if entry.turn_last => (Some(tr.turn_markdown(e)), None, None),
            Kind::Assistant(m) => (Some(m.clone()), None, None),
            Kind::User(t) => (None, Some(t.clone()), None),
            Kind::Tool(t) => (None, None, crate::transcript::unified_diff(t)),
            _ => return,
        };
        let why = if self.clip.ssh {
            "rich text isn't available over SSH"
        } else {
            "rich text isn't available here"
        };
        match (md, user, diff, btn) {
            (Some(md), _, _, Btn::CopyText) => {
                let d = self.clip.copy_plain(&markdown::plain(&md));
                self.report(d, "text", "");
            }
            (Some(md), _, _, Btn::CopyRich) => {
                let d = self
                    .clip
                    .copy_rich(&markdown::plain(&md), &markdown::html(&md), &md);
                self.report(d, "rich text", &format!("{why}; copied markdown"));
            }
            (Some(md), _, _, Btn::CopyMarkdown) => {
                let d = self.clip.copy_plain(&md);
                self.report(d, "markdown", "");
            }
            (Some(md), _, _, Btn::TableMd(n) | Btn::TableHtml(n)) => {
                let parsed = markdown::render_md(&md, 100_000, false);
                let Some(range) = parsed.tables.get(n).cloned() else {
                    return;
                };
                let src = md[range].trim_end().to_string();
                if btn == Btn::TableMd(n) {
                    let d = self.clip.copy_plain(&src);
                    self.report(d, "table (markdown)", "");
                } else {
                    let html = markdown::html(&src);
                    let d = self.clip.copy_rich(&src, &html, &html);
                    self.report(d, "table (html)", &format!("{why}; copied the HTML source"));
                }
            }
            (Some(md), _, _, Btn::Code(n)) => {
                let parsed = markdown::render_md(&md, 100_000, false);
                let Some(code) = parsed.codes.get(n) else {
                    return;
                };
                let code = code.strip_suffix('\n').unwrap_or(code).to_string();
                let d = self.clip.copy_plain(&code);
                self.report(d, "code", "");
            }
            (_, _, Some(diff), Btn::Diff) => {
                let d = self.clip.copy_plain(&diff);
                self.report(d, "diff", "");
            }
            (_, Some(text), _, Btn::CopyUser) => {
                let d = self.clip.copy_plain(&crate::paste::expand(&text));
                self.report(d, "message", "");
            }
            (_, Some(text), _, Btn::Paste(n)) => {
                if let Some(body) = crate::paste::bodies(&text).get(n) {
                    let title = format!(
                        "{} · read-only",
                        crate::paste::label(n as u32 + 1, body).trim_matches(['[', ']'])
                    );
                    self.overlay = Overlay::Pager(Box::new(Pager::text(title, body)));
                }
            }
            (_, Some(text), _, Btn::Attachment(n)) => {
                if let Some((_, path)) = crate::paste::attachments(&text).get(n) {
                    self.open_file(std::path::Path::new(path));
                }
            }
            (_, Some(text), _, Btn::Fork) => self.fork_at_entry(e, &text),
            _ => {}
        }
    }

    /// Fork before user message `e` through the fork-at-message flow.
    fn fork_at_entry(&mut self, e: usize, text: &str) {
        if self.view.is_some() {
            return self.hint("fork here works on root's messages; esc back to root");
        }
        if self.running {
            return self.hint("cancel the running turn first");
        }
        // The k-th equal message from the end of the transcript is the k-th
        // equal candidate from the end of the session.
        let later = self.transcript.entries[e + 1..]
            .iter()
            .filter(|x| matches!(&x.kind, Kind::User(t) if t == text))
            .count();
        let candidates = self.engine.fork_candidates();
        let Some(c) = candidates
            .iter()
            .rev()
            .filter(|c| c.text == text)
            .nth(later)
            .cloned()
        else {
            return self.hint("this message has no fork point (it is not in the session history)");
        };
        let source = self.engine.session_id();
        self.switch_refill(
            crate::engine::Target::Fork(source, Some((c.context, c.line))),
            "forked to",
            Some(c.text),
        );
    }

    /// A file (a saved output, a finding location) in the read-only pager.
    pub(super) fn open_in_pager(&mut self, path: &std::path::Path, line: Option<usize>) {
        let path = if path.is_relative() {
            self.engine.cwd().join(path)
        } else {
            path.to_path_buf()
        };
        match Pager::file(&path, line) {
            Ok(p) => {
                let prev = std::mem::replace(&mut self.overlay, Overlay::Pager(Box::new(p)));
                self.overlay_back =
                    (!matches!(prev, Overlay::None | Overlay::Pager(_))).then_some(prev);
            }
            Err(e) => self.hint(e),
        }
    }

    /// Open a file in the system viewer, with a hint either way.
    pub(super) fn open_file(&mut self, path: &std::path::Path) {
        match crate::attach::open(path) {
            Ok(()) => self.hint(format!(
                "opened {}",
                path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned()
                )
            )),
            Err(e) => self.hint(e),
        }
    }

    /// A click on composer paste chip `b`: its menu.
    pub(super) fn chip_menu(&mut self, b: usize, y: u16) {
        let x = self
            .chip_rects
            .iter()
            .find(|(_, i)| *i == b)
            .map_or(self.comp_area.x, |(r, _)| r.x);
        let attachment = self
            .composer
            .buf
            .blocks
            .get(b)
            .is_some_and(|x| x.attach.is_some());
        let items = if attachment {
            vec![Btn::ChipOpen(b), Btn::ChipExpand(b), Btn::ChipRemove(b)]
        } else {
            vec![
                Btn::ChipView(b),
                Btn::ChipEdit(b),
                Btn::ChipExpand(b),
                Btn::ChipRemove(b),
            ]
        };
        // Above the chip: the composer sits at the bottom.
        let y = y.saturating_sub(items.len() as u16 + 2);
        self.overlay = Overlay::Menu(Box::new(Menu {
            x,
            y,
            entry: usize::MAX,
            items,
            sel: 0,
        }));
    }

    fn context_menu(&mut self, x: u16, y: u16) {
        let a = self.layout.area;
        if !(y >= a.y && y < a.bottom() && x >= a.x && x < a.right()) {
            return;
        }
        let Some((e, _)) = self.layout.locate(self.layout.abs_at(y)) else {
            return;
        };
        let items = self.entry_buttons(e);
        if items.is_empty() {
            return;
        }
        if matches!(self.view_transcript().entries[e].kind, Kind::User(_)) {
            self.view_transcript().set_anchor(Some(e));
        }
        self.overlay = Overlay::Menu(Box::new(Menu {
            x,
            y,
            entry: e,
            items,
            sel: 0,
        }));
    }

    /// The actions an entry offers (buttons, menu items, keyboard focus).
    pub(super) fn entry_buttons(&mut self, e: usize) -> Vec<Btn> {
        let tr = self.view_transcript();
        let Some(entry) = tr.entries.get(e) else {
            return vec![];
        };
        match &entry.kind {
            Kind::Assistant(md) => {
                let mut v = vec![Btn::CopyText, Btn::CopyRich, Btn::CopyMarkdown];
                let parsed = markdown::render_md(md, 100_000, false);
                v.extend((0..parsed.codes.len()).map(Btn::Code));
                for n in 0..parsed.tables.len() {
                    v.push(Btn::TableMd(n));
                    v.push(Btn::TableHtml(n));
                }
                v
            }
            Kind::User(text) => {
                let mut v = vec![Btn::Fork, Btn::CopyUser];
                v.extend((0..crate::paste::bodies(text).len()).map(Btn::Paste));
                v.extend((0..crate::paste::attachments(text).len()).map(Btn::Attachment));
                v
            }
            Kind::Tool(t) if matches!(t.name.as_str(), "write" | "edit") => vec![Btn::Diff],
            Kind::Tool(t)
                if t.output
                    .as_deref()
                    .and_then(crate::transcript::full_output)
                    .is_some() =>
            {
                vec![Btn::Output]
            }
            _ => vec![],
        }
    }

    // ---- keyboard focus -------------------------------------------------

    /// Alt+Up/Down: the previous/next user message becomes the anchor and
    /// scrolls to the top.
    pub(super) fn jump_prompt(&mut self, up: bool) {
        let from = self.focus.map(|f| f.entry).or_else(|| {
            self.layout
                .locate(self.layout.top)
                .map(|(i, _)| i)
                .or(Some(usize::MAX))
        });
        let users = self.view_transcript().user_entries();
        let next = match (up, from) {
            (true, Some(f)) => users.iter().rev().find(|&&i| i < f).copied(),
            (false, Some(f)) => users.iter().find(|&&i| i > f && f != usize::MAX).copied(),
            _ => None,
        };
        let Some(e) = next else {
            return self.hint(if up {
                "no earlier message"
            } else {
                "no later message"
            });
        };
        self.view_transcript().set_anchor(Some(e));
        self.focus = Some(Focus {
            entry: e,
            button: Some(Btn::Fork),
        });
        self.anchor_req = Some((e, 0, 0));
    }

    /// Shift+Alt+Up/Down: focus the previous/next entry with actions or a box.
    pub(super) fn jump_entry(&mut self, up: bool) {
        let n = self.view_transcript().entries.len();
        let from = self
            .focus
            .map(|f| f.entry as isize)
            .unwrap_or(if up { n as isize } else { -1 });
        let mut i = from;
        loop {
            i += if up { -1 } else { 1 };
            if i < 0 || i >= n as isize {
                return self.hint("no more entries");
            }
            let e = i as usize;
            let buttons = self.entry_buttons(e);
            let tr = self.view_transcript();
            let actionable = !buttons.is_empty() || tr.entries[e].selectable();
            if actionable {
                self.focus = Some(Focus {
                    entry: e,
                    button: buttons.first().copied(),
                });
                // Keep it in view without moving the header off-screen.
                let l = &self.layout;
                if let Some(&s) = l.starts.get(e)
                    && (s < l.top || s >= l.top + l.view_h)
                {
                    self.anchor_req = Some((e, 0, (l.view_h / 3) as isize));
                }
                return;
            }
        }
    }

    /// Keys while an entry has focus and the composer is empty; false
    /// passes the key on.
    pub(super) fn focus_key(&mut self, key: KeyEvent) -> bool {
        let Some(f) = self.focus else { return false };
        if f.entry >= self.view_transcript().entries.len() {
            self.focus = None;
            return false;
        }
        let buttons = self.entry_buttons(f.entry);
        let pos = f.button.and_then(|b| buttons.iter().position(|x| *x == b));
        match key.code {
            KeyCode::Left | KeyCode::Right if !buttons.is_empty() => {
                let n = buttons.len();
                let p = pos.unwrap_or(0);
                let next = if key.code == KeyCode::Left {
                    (p + n - 1) % n
                } else {
                    (p + 1) % n
                };
                self.focus = Some(Focus {
                    button: Some(buttons[next]),
                    ..f
                });
            }
            KeyCode::Enter => match f.button {
                Some(b) => self.run_button(f.entry, b),
                None => self.toggle_focused(f.entry),
            },
            KeyCode::Char(' ') => self.toggle_focused(f.entry),
            KeyCode::Up | KeyCode::Down => {
                let d = if key.code == KeyCode::Up { -1 } else { 1 };
                let tr = self.view_transcript();
                if !tr.entries[f.entry].scroll_box(d) {
                    self.scroll_by(d);
                }
            }
            KeyCode::Esc => {
                self.focus = None;
                self.view_transcript().set_anchor(None);
            }
            _ => {
                self.focus = None;
                return false;
            }
        }
        true
    }

    fn toggle_focused(&mut self, e: usize) {
        let l = &self.layout;
        let y = l
            .starts
            .get(e)
            .map(|&s| (l.area.y as isize + s as isize - l.top as isize).max(0) as u16)
            .unwrap_or(l.area.y);
        self.toggle_entry(e, 0, y);
    }
}
