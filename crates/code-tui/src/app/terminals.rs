//! The terminal sidebar (D144 private user terminals, D140 tty jobs).
//!
//! Layout: at 100+ columns a resizable right-hand column beside the chat
//! (drag its left edge, or `<` / `>` with the panel focused); narrower, the
//! panel covers the chat area full-width above the composer, which stays
//! visible. Tabs list every open terminal (at most eight).
//!
//! Focus: composer → ctrl+] → typing in the terminal (every key goes to the
//! PTY) → ctrl+] → panel keys (tabs, share, close…) → ctrl+]/esc → composer.
//! A click on the screen types there; a click elsewhere leaves it.
//!
//! Privacy (D144): terminal bytes arrive on their own channel
//! ([`TermMsg`]), feed only the view's own `vt100` screen, and are purged
//! when the terminal closes. They never become a UiEvent, transcript row,
//! search hit, export, pager text, copy/OSC 52 payload or log line.
//! Selecting rows is local and only stages an excerpt for sharing; the
//! clipboard is never touched from here. Transcript rows carry metadata only
//! (who asked, prompt, share mode, status, the user's note).
use super::*;
use crate::engine::{TermCmd, TermInfo, TermMsg, TermRx, TermTx};
use crate::termview::{TermSel, TermView, is_leave_key, key_bytes};
use crate::transcript::{Card, CardPart, Tone};
use crate::ui::TermAct;
use ratatui::text::Text;
use ratatui::widgets::Wrap;
use unicode_width::UnicodeWidthStr;

/// At most this many tabs (the engine caps open user terminals at eight).
const MAX_TABS: usize = 8;
/// The narrowest panel and chat beside it.
const MIN_PANEL: u16 = 36;
const MIN_CHAT: u16 = 40;
/// Largest excerpt the engine accepts.
const SHARE_CAP: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TermFocus {
    None,
    /// Panel keys: tabs, share, close.
    Panel,
    /// Keys go to the PTY.
    Typing,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TermDrag {
    Edge,
    /// A row selection from this absolute row; moved once dragged.
    Select {
        anchor: usize,
        moved: bool,
    },
}

pub(super) struct Terms {
    views: Vec<TermView>,
    active: usize,
    pub show: bool,
    pub focus: TermFocus,
    /// Preferred panel width in the wide layout.
    width: u16,
    rx: Option<TermRx>,
    tx: Option<TermTx>,
    /// Panel and screen rects from the last draw.
    pub rect: Rect,
    body: Rect,
    drag: Option<TermDrag>,
    /// A keyboard row selection is being extended (↑↓, enter shares).
    kbd_sel: bool,
    /// The close note per terminal (shared with the agent anyway).
    notes: BTreeMap<String, Option<String>>,
    /// A private request opened the panel; it hides again when the last
    /// private terminal closes (unless the user toggled it meanwhile).
    auto_shown: bool,
}

impl Default for Terms {
    fn default() -> Self {
        Self {
            views: vec![],
            active: 0,
            show: false,
            focus: TermFocus::None,
            width: 0,
            rx: None,
            tx: None,
            rect: Rect::default(),
            body: Rect::default(),
            drag: None,
            kbd_sel: false,
            notes: BTreeMap::new(),
            auto_shown: false,
        }
    }
}

impl Terms {
    pub fn visible(&self) -> bool {
        self.show && !self.views.is_empty()
    }
    /// Open private terminals (for quit and switch confirmations).
    pub fn open_private(&self) -> usize {
        self.views
            .iter()
            .filter(|v| v.info.user && v.info.status != "closed")
            .count()
    }
    pub fn count(&self) -> usize {
        self.views.len()
    }
    /// Any private terminal is waiting on the user.
    pub fn waiting(&self) -> bool {
        self.open_private() > 0
    }
    pub fn rx_mut(&mut self) -> &mut Option<TermRx> {
        &mut self.rx
    }
    fn index(&self, id: &str) -> Option<usize> {
        self.views.iter().position(|v| v.info.id == id)
    }
    fn send(&self, cmd: TermCmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }
}

pub async fn recv_term(rx: &mut Option<TermRx>) -> Option<TermMsg> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

fn mode_text(info: &TermInfo) -> &'static str {
    if info.share_output {
        "output shared on close"
    } else {
        "status only"
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

fn fmt_size(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.1} KiB", n as f64 / 1024.0)
    }
}

impl App {
    pub fn term_events(&mut self) -> &mut Option<TermRx> {
        &mut self.terms.rx
    }

    /// A new or switched-to session: drop every view (purging private
    /// screens), subscribe, then pick up live terminals.
    pub(super) fn terms_reset(&mut self) {
        for v in &mut self.terms.views {
            v.purge();
        }
        let width = self.terms.width;
        self.terms = Terms {
            width,
            ..Terms::default()
        };
        let (rx, tx) = self.engine.terminal_channels(self.async_errors.0.clone());
        self.terms.rx = Some(rx);
        self.terms.tx = Some(tx);
        for info in self.engine.terminals() {
            if info.status != "closed" && (info.user || info.alive()) {
                self.term_add(info);
            }
        }
        if let Some(e) = self.engine.terminal_issue.take() {
            self.transcript.notice(Level::Error, e);
        }
    }

    fn term_add(&mut self, mut info: TermInfo) -> usize {
        info.breadcrumb = self.tree.crumb(&info.breadcrumb);
        // Make room: the oldest finished job tab goes first.
        while self.terms.views.len() >= MAX_TABS {
            let Some(i) = self
                .terms
                .views
                .iter()
                .position(|v| !v.info.user && !v.info.alive())
            else {
                break;
            };
            self.term_remove(i);
        }
        let (rows, cols) = self.term_screen_size();
        let mut view = TermView::new(info, rows, cols);
        let id = view.info.id.clone();
        if view.info.user {
            if let Ok(s) = self.engine.terminal_snapshot(&id) {
                view.reset(s.offset, &s.bytes, s.truncated);
            }
        } else {
            Self::job_catch_up(&self.engine, &mut view, true);
        }
        view.unread = false;
        self.terms.views.push(view);
        self.terms.views.len() - 1
    }

    /// Fill a job view from its output log after a gap (or from the start,
    /// keeping at most the last 256 KiB).
    fn job_catch_up(engine: &Engine, view: &mut TermView, fresh: bool) {
        let id = view.info.id.clone();
        let mut offset = view.next;
        if fresh && let Ok(first) = engine.terminal_job_page(&id, 0) {
            let total = first.total;
            offset = total.saturating_sub(256 * 1024);
            if offset == 0 {
                view.reset(0, &first.bytes, false);
                offset = first.next;
                if first.eof {
                    return;
                }
            } else {
                view.reset(offset, &[], true);
            }
        }
        for _ in 0..64 {
            let Ok(page) = engine.terminal_job_page(&id, offset) else {
                return;
            };
            if page.next <= offset {
                return;
            }
            view.feed(offset, &page.bytes);
            view.next = page.next;
            offset = page.next;
            if page.eof {
                return;
            }
        }
    }

    fn term_remove(&mut self, i: usize) {
        let mut v = self.terms.views.remove(i);
        v.purge();
        drop(v);
        if self.terms.active >= i && self.terms.active > 0 {
            self.terms.active -= 1;
        }
        if self.terms.views.is_empty() {
            self.terms.show = false;
            self.terms.focus = TermFocus::None;
            self.terms.active = 0;
        }
        self.terms.kbd_sel = false;
        self.terms.drag = None;
    }

    /// Terminal bytes (or a lag notice) for the views.
    pub fn on_term_msg(&mut self, msg: Option<TermMsg>) {
        let Some(msg) = msg else {
            self.terms.rx = None;
            return;
        };
        match msg {
            TermMsg::Output { id, offset, bytes } => {
                let Some(i) = self.terms.index(&id) else {
                    // Unknown yet: the metadata's snapshot covers it.
                    return;
                };
                let ok = self.terms.views[i].feed(offset, &bytes);
                drop(bytes);
                if !ok {
                    self.term_resync(i);
                }
                if self.terms.visible() {
                    if i == self.terms.active {
                        self.terms.views[i].unread = false;
                    }
                    self.dirty = true;
                }
            }
            TermMsg::Lagged => {
                for i in 0..self.terms.views.len() {
                    self.term_resync(i);
                }
                self.dirty = true;
            }
        }
    }

    /// Recover after lost chunks: a private terminal resets from its
    /// retained tail (marked as truncated), a job pages from its offset.
    fn term_resync(&mut self, i: usize) {
        let view = &mut self.terms.views[i];
        if view.info.user {
            if view.info.status == "closed" {
                return;
            }
            match self.engine.terminal_snapshot(&view.info.id) {
                Ok(s) => {
                    view.reset(s.offset, &s.bytes, true);
                }
                Err(_) => view.lost = true,
            }
        } else {
            Self::job_catch_up(&self.engine, view, false);
        }
    }

    /// Terminal metadata changed (opened, share mode, staged excerpt,
    /// exited, closed).
    pub(super) fn on_terminal(&mut self, mut info: TermInfo) {
        // Display labels (`root › build#1`) for the engine's id breadcrumb.
        info.breadcrumb = self.tree.crumb(&info.breadcrumb);
        let Some(i) = self.terms.index(&info.id) else {
            if info.status == "closed" || (!info.user && !info.alive()) {
                return;
            }
            return self.term_opened(info);
        };
        let prev = self.terms.views[i].info.clone();
        if info.user && info.status == "closed" {
            return self.term_closed(i, info);
        }
        let view = &mut self.terms.views[i];
        // More private: drop the local selection (the engine revoked any
        // staged excerpt).
        let downgraded = prev.share_output && !info.share_output && info.status != "closed";
        if downgraded {
            view.sel = None;
            view.made_private = true;
            self.terms.kbd_sel = false;
        }
        if info.status != "closed" {
            view.last_output = info.share_output;
            view.last_staged = info.staged;
        }
        // (A close of a running shell reports exited just before closed.)
        let exited =
            info.user && !view.closing && prev.status == "running" && info.status == "exited";
        view.info = info;
        if downgraded {
            let mut card = open_card(&view.info);
            card.head.push_str(" · made private by you");
            let agent = view.info.agent.clone();
            self.term_card(&agent, card);
        }
        let view = &mut self.terms.views[i];
        if exited {
            if self.terms.focus == TermFocus::Typing && i == self.terms.active {
                self.terms.focus = TermFocus::Panel;
            }
            // A cancel or shutdown reports exited just before closed: tell
            // the user only once it is still waiting a moment later.
            view.exited_at = Some(Instant::now());
        }
    }

    /// Every tick: a shell that exited by itself waits for the user's close.
    pub(super) fn term_tick(&mut self) -> bool {
        let Some(view) = self.terms.views.iter_mut().find(|v| {
            v.exited_at
                .is_some_and(|t| t.elapsed() > Duration::from_millis(400))
        }) else {
            return false;
        };
        view.exited_at = None;
        if view.info.status != "exited" || view.closing {
            return false;
        }
        let crumb = view.info.breadcrumb.clone();
        self.hint(format!(
            "terminal for {crumb}: the shell exited · close it to send the result"
        ));
        self.attend("terminal waiting");
        true
    }

    fn term_opened(&mut self, info: TermInfo) {
        let user = info.user;
        let whenever = info.whenever;
        let crumb = info.breadcrumb.clone();
        if user {
            let card = open_card(&info);
            self.term_card(&info.agent, card);
        }
        let i = self.term_add(info);
        if !user {
            if !self.terms.show {
                self.hint(format!(
                    "{crumb} started a tty job · ctrl+] or /terminal to watch or type in it"
                ));
            }
            return;
        }
        // A private request: show it, ring, and take focus only when the
        // user is idle (never mid-typing, never for whenever requests).
        if !self.terms.show {
            self.terms.auto_shown = true;
        }
        self.terms.show = true;
        let typing = self.terms.focus == TermFocus::Typing;
        if !typing {
            self.terms.active = i;
        }
        let idle = self.last_key.elapsed() > Duration::from_secs(2)
            && self.composer.is_empty()
            && matches!(self.overlay, Overlay::None);
        if !whenever && !typing && idle {
            self.terms.focus = TermFocus::Typing;
            self.hint("typing in the terminal · ctrl+] leaves");
        } else {
            self.hint(format!(
                "{crumb} opened a private terminal · ctrl+] to type in it"
            ));
        }
        self.attend("terminal waiting");
    }

    fn term_closed(&mut self, i: usize, info: TermInfo) {
        let view = &self.terms.views[i];
        let by_user = view.closing;
        let note = self.terms.notes.remove(&info.id).flatten();
        let shared = if !by_user {
            "output not shared".to_string()
        } else if view.last_staged > 0 {
            format!("excerpt shared ({})", fmt_size(view.last_staged))
        } else if view.last_output {
            "output shared (tail)".to_string()
        } else if view.made_private {
            "output not shared (made private)".to_string()
        } else {
            "output not shared".to_string()
        };
        let status = match info.exit {
            Some(code) => format!("exit {code}"),
            None if by_user => "closed".to_string(),
            None => "closed (cancelled or session ended)".to_string(),
        };
        let mut head = format!(
            "terminal closed · {} · {status} · {shared}",
            info.breadcrumb
        );
        if let Some(n) = &note {
            head.push_str(&format!(" · note: {}", first_line(n)));
        }
        let card = Card {
            key: format!("{}:close", info.id),
            icon: "⌨".into(),
            tone: if by_user { Tone::Ok } else { Tone::Warn },
            head,
            body: note
                .filter(|n| n.contains('\n'))
                .map(|n| {
                    vec![CardPart::Md(format!(
                        "**Your note** (sent to the agent)\n\n{n}"
                    ))]
                })
                .unwrap_or_default(),
        };
        self.term_card(&info.agent, card);
        let was_active = i == self.terms.active;
        self.term_remove(i);
        if self.terms.auto_shown && self.terms.open_private() == 0 {
            self.terms.auto_shown = false;
            self.terms.show = false;
            self.terms.focus = TermFocus::None;
        }
        if was_active && self.terms.focus != TermFocus::None {
            self.terms.focus = if self.terms.views.is_empty() {
                TermFocus::None
            } else {
                TermFocus::Panel
            };
        }
    }

    /// A metadata row in the asker's transcript (and root's for a child).
    fn term_card(&mut self, agent: &str, card: Card) {
        if agent != "root" {
            self.transcript.card(card.clone());
        }
        if let Some(tr) = self.transcript_of(agent) {
            tr.card(card);
        }
    }

    // ---- commands and keys -------------------------------------------

    /// `/terminal`: show or hide the panel.
    pub(super) fn term_toggle(&mut self) {
        if self.terms.views.is_empty() {
            return self.hint(
                "no terminals · agents open them with the terminal tool; tty jobs show here too",
            );
        }
        self.terms.show = !self.terms.show;
        self.terms.auto_shown = false;
        if !self.terms.show {
            self.terms.focus = TermFocus::None;
        }
    }

    /// Ctrl+] from the composer: type in the active terminal.
    pub(super) fn term_enter(&mut self) {
        if self.terms.views.is_empty() {
            return self.hint("no terminals open");
        }
        self.terms.show = true;
        let alive = self.terms.views[self.terms.active].info.alive();
        self.terms.focus = if alive {
            TermFocus::Typing
        } else {
            TermFocus::Panel
        };
    }

    /// Keys while the panel or terminal has focus. True when consumed.
    pub(super) fn term_key(&mut self, key: KeyEvent) -> bool {
        if !self.terms.visible() {
            self.terms.focus = TermFocus::None;
        }
        match self.terms.focus {
            TermFocus::None => {
                if is_leave_key(&key) {
                    self.term_enter();
                    return true;
                }
                false
            }
            TermFocus::Typing => {
                if is_leave_key(&key) {
                    self.terms.focus = TermFocus::Panel;
                    return true;
                }
                let view = &mut self.terms.views[self.terms.active];
                if !view.info.alive() {
                    self.terms.focus = TermFocus::Panel;
                    return true;
                }
                if let Some(bytes) = key_bytes(key, view.app_cursor()) {
                    view.scroll_to(0);
                    let chars = String::from_utf8_lossy(&bytes).into_owned();
                    let id = view.info.id.clone();
                    self.terms.send(TermCmd::Input(id, chars));
                }
                true
            }
            TermFocus::Panel => self.term_panel_key(key),
        }
    }

    fn term_panel_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            // Ctrl+C keeps its app meaning (cancel the turn / quit).
            return false;
        }
        if self.terms.kbd_sel {
            let view = &mut self.terms.views[self.terms.active];
            let last = view.last_abs();
            match key.code {
                KeyCode::Up | KeyCode::Down => {
                    if let Some(s) = &mut view.sel {
                        s.focus = if key.code == KeyCode::Up {
                            s.focus.saturating_sub(1)
                        } else {
                            (s.focus + 1).min(last)
                        };
                        let f = s.focus;
                        view.reveal(f);
                    }
                }
                KeyCode::Enter | KeyCode::Char('s') => {
                    self.terms.kbd_sel = false;
                    self.term_act(TermAct::Share);
                }
                KeyCode::Esc => {
                    self.terms.kbd_sel = false;
                    view.sel = None;
                }
                _ => {}
            }
            return true;
        }
        let n = self.terms.views.len();
        match key.code {
            _ if is_leave_key(&key) => self.terms.focus = TermFocus::None,
            KeyCode::Esc => {
                if self.terms.views[self.terms.active].sel.take().is_none() {
                    self.terms.focus = TermFocus::None;
                }
            }
            KeyCode::Enter | KeyCode::Char('i') => {
                if self.terms.views[self.terms.active].info.alive() {
                    self.terms.focus = TermFocus::Typing;
                } else {
                    self.hint("this terminal is not running");
                }
            }
            KeyCode::Left | KeyCode::Char('[') => self.term_tab((self.terms.active + n - 1) % n),
            KeyCode::Right | KeyCode::Char(']') | KeyCode::Tab => {
                self.term_tab((self.terms.active + 1) % n)
            }
            KeyCode::Char(c @ '1'..='8') => {
                let i = c as usize - '1' as usize;
                if i < n {
                    self.term_tab(i);
                }
            }
            KeyCode::Char('p') => self.term_act(TermAct::Private),
            KeyCode::Char('s') => {
                let view = &mut self.terms.views[self.terms.active];
                if !view.info.user {
                    self.hint("tty job output is already visible to the agent");
                } else if view.sel.is_some() {
                    self.term_act(TermAct::Share);
                } else {
                    let row = view.cursor_abs();
                    view.sel = Some(TermSel {
                        anchor: row,
                        focus: row,
                    });
                    view.reveal(row);
                    self.terms.kbd_sel = true;
                }
            }
            KeyCode::Char('r') => self.term_act(TermAct::Revoke),
            KeyCode::Char('c') => self.term_act(TermAct::Close),
            KeyCode::Char('k') => self.term_act(TermAct::Kill),
            KeyCode::Char('x') => self.term_act(TermAct::Dismiss),
            KeyCode::Char('h') => self.term_act(TermAct::Hide),
            KeyCode::Char('<') => self.terms.width = self.terms.rect.width.saturating_add(4),
            KeyCode::Char('>') => {
                self.terms.width = self.terms.rect.width.saturating_sub(4).max(MIN_PANEL)
            }
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Up | KeyCode::Down => {
                let page = self.terms.body.height.saturating_sub(1).max(1) as isize;
                let delta = match key.code {
                    KeyCode::PageUp => -page,
                    KeyCode::PageDown => page,
                    KeyCode::Up => -1,
                    _ => 1,
                };
                self.terms.views[self.terms.active].scroll_by(delta);
            }
            KeyCode::End => self.terms.views[self.terms.active].scroll_to(0),
            _ => {}
        }
        true
    }

    fn term_tab(&mut self, i: usize) {
        if i < self.terms.views.len() {
            self.terms.active = i;
            self.terms.kbd_sel = false;
            self.terms.views[i].unread = false;
        }
    }

    /// A bracketed paste while typing in the terminal. True when consumed.
    pub(super) fn term_paste(&mut self, text: &str) -> bool {
        if self.terms.focus != TermFocus::Typing || !self.terms.visible() {
            return false;
        }
        let view = &self.terms.views[self.terms.active];
        if !view.info.alive() {
            return true;
        }
        let body = text.replace('\n', "\r");
        let chars = if view.bracketed_paste() {
            format!("\x1b[200~{body}\x1b[201~")
        } else {
            body
        };
        let id = view.info.id.clone();
        // The engine takes at most 64 KiB per write.
        let mut rest = chars.as_str();
        while !rest.is_empty() {
            let mut cut = rest.len().min(60 * 1024);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            self.terms
                .send(TermCmd::Input(id.clone(), rest[..cut].to_string()));
            rest = &rest[cut..];
        }
        true
    }

    fn term_act(&mut self, act: TermAct) {
        if self.terms.views.is_empty() {
            return;
        }
        let i = self.terms.active;
        let id = self.terms.views[i].info.id.clone();
        let user = self.terms.views[i].info.user;
        match act {
            TermAct::Tab(t) => self.term_tab(t),
            TermAct::Hide => {
                self.terms.show = false;
                self.terms.auto_shown = false;
                self.terms.focus = TermFocus::None;
            }
            TermAct::Private => {
                if !user || !self.terms.views[i].info.share_output {
                    return self.hint("already status only");
                }
                match self.engine.terminal_make_private(&id) {
                    Ok(()) => {
                        self.terms.views[i].sel = None;
                        self.hint("status only: the agent gets the exit status and your note");
                    }
                    Err(e) => self.hint(e),
                }
            }
            TermAct::Share => {
                if !user {
                    return;
                }
                let Some(text) = self.terms.views[i].selection_text() else {
                    return self.hint("select rows first: drag over them, or s then ↑↓");
                };
                if text.len() > SHARE_CAP {
                    return self.hint("the selection is over 64 KiB · select fewer rows");
                }
                match self.engine.terminal_stage(&id, Some(text)) {
                    Ok(()) => self.hint("excerpt staged · sent only when you close · r revokes"),
                    Err(e) => self.hint(e),
                }
            }
            TermAct::Revoke => {
                if !user || self.terms.views[i].info.staged == 0 {
                    return self.hint("nothing staged");
                }
                match self.engine.terminal_stage(&id, None) {
                    Ok(()) => {
                        self.terms.views[i].sel = None;
                        self.hint("excerpt revoked");
                    }
                    Err(e) => self.hint(e),
                }
            }
            TermAct::Close => {
                if user {
                    self.term_close_dialog(i)
                } else if self.terms.views[i].info.alive() {
                    self.hint("k kills the job; x removes a finished job's tab")
                } else {
                    self.term_remove(i)
                }
            }
            TermAct::Kill => {
                if user {
                    return self.term_close_dialog(i);
                }
                if self.terms.views[i].info.alive() {
                    self.engine.kill_job(id);
                    self.hint("killing job…");
                }
            }
            TermAct::Dismiss => {
                if !user && !self.terms.views[i].info.alive() {
                    self.term_remove(i);
                } else if user {
                    self.hint("c closes a private terminal");
                } else {
                    self.hint("the job is still running · k kills it");
                }
            }
            TermAct::Body | TermAct::Edge => {}
        }
    }

    fn term_close_dialog(&mut self, i: usize) {
        let info = &self.terms.views[i].info;
        let sharing = if info.staged > 0 {
            format!(
                "the **excerpt you staged** ({}), the exit status and your note",
                fmt_size(info.staged)
            )
        } else if info.share_output {
            "**the last 64 KiB of output**, the exit status and your note".to_string()
        } else {
            "only the **exit status** (or `closed`) and your note; no output".to_string()
        };
        let status = if info.status == "exited" {
            match info.exit {
                Some(c) => format!("The shell exited with status {c}."),
                None => "The shell exited.".to_string(),
            }
        } else {
            "The shell is still running; closing ends it (status `closed`).".to_string()
        };
        let dialog = Dialog::new(
            format!("Close terminal · {}", info.breadcrumb),
            Body::Markdown(format!(
                "{status}\n\n{} receives {sharing}. The screen and scrollback are discarded.",
                info.breadcrumb
            )),
            vec![
                DialogOption::new("close", "Close and send"),
                DialogOption::new("stay", "Keep it open"),
            ],
        )
        .with_text("optional note for the agent · enter closes and sends · esc keeps it open")
        .text_option("close", true);
        self.overlay = Overlay::Dialog(
            Box::new(dialog),
            DialogPurpose::TerminalClose(info.id.clone()),
        );
    }

    pub(super) fn term_close_chosen(&mut self, id: &str, note: Option<String>) {
        let Some(i) = self.terms.index(id) else {
            return self.hint("that terminal already closed");
        };
        let note = note.filter(|n| !n.trim().is_empty());
        if note.as_ref().is_some_and(|n| n.len() > 8192) {
            return self.hint("the note is over 8 KiB");
        }
        self.terms.views[i].closing = true;
        self.terms.notes.insert(id.to_string(), note.clone());
        self.terms.send(TermCmd::Close(id.to_string(), note));
        self.hint("closing the terminal…");
    }

    // ---- mouse ----------------------------------------------------------

    /// Mouse events over (or dragging from) the panel. True when consumed.
    pub(super) fn term_mouse(&mut self, m: MouseEvent) -> bool {
        let (x, y) = (m.column, m.row);
        if self.terms.views.is_empty() {
            self.terms.drag = None;
        }
        if let Some(drag) = self.terms.drag {
            match m.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.term_drag_to(drag, x, y);
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.terms.drag = None;
                    if let TermDrag::Select { moved: false, .. } = drag
                        && let Some(v) = self.terms.views.get_mut(self.terms.active)
                    {
                        v.sel = None;
                    } else if let TermDrag::Select { .. } = drag {
                        self.terms.focus = TermFocus::Panel;
                        self.hint("s stages the selected rows for sharing · esc clears");
                    }
                    return true;
                }
                _ => {}
            }
        }
        if !self.terms.visible() || !matches!(self.overlay, Overlay::None) {
            return false;
        }
        let r = self.terms.rect;
        let inside = x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        if !inside {
            if matches!(m.kind, MouseEventKind::Down(_)) && self.terms.focus != TermFocus::None {
                self.terms.focus = TermFocus::None;
                self.terms.kbd_sel = false;
            }
            return false;
        }
        let b = self.terms.body;
        let in_body = x >= b.x && x < b.right() && y >= b.y && y < b.bottom();
        match m.kind {
            MouseEventKind::Moved => false,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                let view = &mut self.terms.views[self.terms.active];
                if in_body && view.alternate() && view.info.alive() {
                    // Full-screen programs (less, vim) scroll by arrow keys.
                    let key = KeyEvent::new(
                        if up { KeyCode::Up } else { KeyCode::Down },
                        KeyModifiers::NONE,
                    );
                    if let Some(bytes) = key_bytes(key, view.app_cursor()) {
                        let chars = String::from_utf8_lossy(&bytes).repeat(3);
                        let id = view.info.id.clone();
                        self.terms.send(TermCmd::Input(id, chars));
                    }
                } else {
                    view.scroll_by(if up { -3 } else { 3 });
                }
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                match self.hits.at(x, y).map(|(_, t)| t) {
                    Some(Target::Scroll(_)) => return false,
                    Some(Target::Term(TermAct::Edge)) => {
                        self.terms.drag = Some(TermDrag::Edge);
                    }
                    Some(Target::Term(TermAct::Body)) => {
                        let row = y - b.y;
                        let view = &mut self.terms.views[self.terms.active];
                        let abs = view.abs_row(row);
                        view.sel = None;
                        self.terms.kbd_sel = false;
                        self.terms.drag = Some(TermDrag::Select {
                            anchor: abs,
                            moved: false,
                        });
                        self.terms.focus = if view.info.alive() {
                            TermFocus::Typing
                        } else {
                            TermFocus::Panel
                        };
                    }
                    // Tabs and chips act without moving keyboard focus:
                    // typing after a click must never land in a terminal.
                    Some(Target::Term(act)) => self.term_act(act),
                    _ => {}
                }
                true
            }
            // No context menu and nothing else over a terminal.
            _ => true,
        }
    }

    fn term_drag_to(&mut self, drag: TermDrag, x: u16, y: u16) {
        match drag {
            TermDrag::Edge => {
                let right = self.terms.rect.right();
                self.terms.width = right.saturating_sub(x).max(MIN_PANEL);
            }
            TermDrag::Select { anchor, .. } => {
                let b = self.terms.body;
                let view = &mut self.terms.views[self.terms.active];
                // Past an edge the view scrolls along.
                if y < b.y {
                    view.scroll_by(-1);
                } else if y >= b.bottom() {
                    view.scroll_by(1);
                }
                let row = y.clamp(b.y, b.bottom().saturating_sub(1)) - b.y;
                let focus = view.abs_row(row);
                view.sel = Some(TermSel { anchor, focus });
                self.terms.drag = Some(TermDrag::Select {
                    anchor,
                    moved: true,
                });
            }
        }
    }

    pub(super) fn term_set_scroll(&mut self, off: usize) {
        if let Some(v) = self.terms.views.get_mut(self.terms.active) {
            let (len, _) = v.scroll_state();
            v.scroll_to(len.saturating_sub(off));
        }
    }

    // ---- layout and drawing ---------------------------------------------

    /// The screen size for a new view: the last body, else 24×80.
    fn term_screen_size(&self) -> (u16, u16) {
        let b = self.terms.body;
        if b.width > 1 && b.height > 0 {
            (b.height, b.width - 1)
        } else {
            (24, 80)
        }
    }

    /// Split the chat area: Some(panel) when the sidebar shows. In the
    /// wide layout `chat` shrinks; narrow, the panel covers it.
    pub(super) fn term_layout(&mut self, chat: &mut Rect, wide: bool) -> Option<Rect> {
        self.terms.rect = Rect::default();
        if !self.terms.visible() || chat.height < 6 {
            return None;
        }
        if wide && chat.width >= MIN_PANEL + MIN_CHAT {
            let pref = if self.terms.width == 0 {
                chat.width / 2
            } else {
                self.terms.width
            };
            let w = pref.clamp(MIN_PANEL, chat.width - MIN_CHAT);
            let panel = Rect {
                x: chat.right() - w,
                width: w,
                ..*chat
            };
            chat.width -= w;
            Some(panel)
        } else {
            Some(*chat)
        }
    }

    pub(super) fn draw_terms(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme();
        // Whatever was drawn under a covering panel loses its clicks.
        self.hits
            .rects
            .retain(|(r, _)| r.intersection(area).is_empty());
        self.hits
            .regions
            .retain(|(r, _)| r.intersection(area).is_empty());
        self.links.retain(|l| {
            l.y < area.y || l.y >= area.bottom() || l.x + l.text.width() as u16 <= area.x
        });
        frame.render_widget(Clear, area);
        self.terms.rect = area;
        let focused = self.terms.focus != TermFocus::None;
        let edge_style = Style::default().fg(if focused { t.accent } else { t.rule });
        // Left edge (resize handle).
        let edge = Rect { width: 1, ..area };
        let buf = frame.buffer_mut();
        for dy in 0..edge.height {
            if let Some(c) = buf.cell_mut((edge.x, edge.y + dy)) {
                c.set_symbol("│");
                c.set_style(edge_style);
            }
        }
        self.hits.add(edge, Target::Term(TermAct::Edge));
        let inner = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            ..area
        };
        let w = inner.width as usize;
        if w < 10 {
            return;
        }
        let mut y = inner.y;
        // Tabs: full labels when they fit, else compact ones (number and
        // mark), else a window around the active tab.
        let mut x = inner.x;
        let mut tab_spans: Vec<(Rect, Span<'static>, Option<TermAct>)> = vec![];
        let label = Span::styled(
            " ⌨ ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        );
        x += label.content.width() as u16;
        tab_spans.push((Rect::default(), label, None));
        let room = inner.right().saturating_sub(x + 3) as usize;
        let tabs: Vec<(String, String)> = self
            .terms
            .views
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mark = if v.info.user {
                    if v.info.alive() { "●" } else { "◌" }
                } else if v.info.alive() {
                    "⚙"
                } else {
                    "✓"
                };
                let name = if v.info.user {
                    v.info.breadcrumb.clone()
                } else {
                    let cmd = v.info.command.as_deref().map(first_line).unwrap_or("job");
                    truncate(cmd, 14)
                };
                (
                    format!(" {} {mark} {} ", i + 1, truncate(&name, 18)),
                    format!(" {}{mark} ", i + 1),
                )
            })
            .collect();
        let fits = |texts: &mut dyn Iterator<Item = &String>| -> bool {
            texts.map(|s| s.width() + 1).sum::<usize>() <= room
        };
        let compact = !fits(&mut tabs.iter().map(|(f, _)| f));
        let texts: Vec<&String> = tabs
            .iter()
            .map(|(f, c)| if compact { c } else { f })
            .collect();
        // The first tab shown keeps the active one in view.
        let mut first = 0;
        while first < self.terms.active
            && texts[first..=self.terms.active]
                .iter()
                .map(|s| s.width() + 1)
                .sum::<usize>()
                + 2
                > room
        {
            first += 1;
        }
        if first > 0 {
            tab_spans.push((Rect::default(), Span::styled("‹ ", t.faint()), None));
            x += 2;
        }
        for (i, text) in texts.iter().enumerate().skip(first) {
            let v = &self.terms.views[i];
            let active = i == self.terms.active;
            let style = if active {
                Style::default()
                    .fg(t.surface)
                    .bg(if v.info.user { t.green } else { t.yellow })
                    .add_modifier(Modifier::BOLD)
            } else if v.unread {
                Style::default().fg(t.ink).add_modifier(Modifier::BOLD)
            } else {
                t.muted()
            };
            let tw = text.width() as u16;
            if x + tw + 3 > inner.right() {
                tab_spans.push((Rect::default(), Span::styled("›", t.faint()), None));
                break;
            }
            tab_spans.push((
                Rect {
                    x,
                    y,
                    width: tw,
                    height: 1,
                },
                Span::styled(text.to_string(), style),
                Some(TermAct::Tab(i)),
            ));
            x += tw + 1;
            tab_spans.push((Rect::default(), Span::raw(" "), None));
        }
        let mut line: Vec<Span<'static>> = vec![];
        for (rect, span, act) in tab_spans {
            if let Some(a) = act {
                self.hits.add(rect, Target::Term(a));
            }
            line.push(span);
        }
        frame.render_widget(
            Paragraph::new(Line::from(line)),
            Rect {
                y,
                height: 1,
                ..inner
            },
        );
        // Hide chip at the right.
        let hide = Rect {
            x: inner.right().saturating_sub(3),
            y,
            width: 3,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Span::styled(" ✕ ", t.muted())), hide);
        self.hits.add(hide, Target::Term(TermAct::Hide));
        y += 1;
        let Some(view) = self.terms.views.get(self.terms.active) else {
            return;
        };
        let info = view.info.clone();
        // Header.
        let mut head: Vec<Line<'static>> = vec![];
        if info.user {
            head.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", info.breadcrumb),
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if info.whenever {
                        "asks (whenever you like):"
                    } else {
                        "asks you:"
                    },
                    t.muted(),
                ),
            ]));
            let prompt = info.prompt.clone().unwrap_or_default();
            let mut plines = wrap_lines(
                prompt
                    .lines()
                    .map(|l| Line::from(Span::styled(l.to_string(), t.text())))
                    .collect(),
                w.saturating_sub(2),
                &[Span::raw(" ")],
                &[Span::raw(" ")],
            );
            if plines.len() > 4 {
                plines.truncate(4);
                plines.push(Line::from(Span::styled(" …", t.faint())));
            }
            head.extend(plines);
            if let Some(cmd) = info.command.as_deref().filter(|c| !c.is_empty()) {
                head.push(Line::from(vec![
                    Span::styled(" proposed ", t.muted()),
                    Span::styled(
                        truncate(cmd, w.saturating_sub(28)),
                        Style::default().fg(t.code_fg),
                    ),
                    Span::styled("  typed, not run", t.faint()),
                ]));
            }
            head.push(Line::from(vec![
                Span::styled(" cwd ", t.muted()),
                Span::styled(truncate(&info.cwd, w.saturating_sub(6)), t.faint()),
            ]));
            let (badge, bg) = if info.share_output {
                (
                    " ⚠ OUTPUT SHARED ON CLOSE · the agent gets the last 64 KiB ",
                    t.yellow,
                )
            } else {
                (
                    " STATUS ONLY · the agent sees the exit status and your note ",
                    t.green,
                )
            };
            head.push(Line::from(Span::styled(
                truncate(badge, w),
                Style::default()
                    .fg(t.surface)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            )));
            if info.staged > 0 {
                head.push(Line::from(Span::styled(
                    format!(
                        " ✂ excerpt staged: {} · sent at close ",
                        fmt_size(info.staged)
                    ),
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                )));
            }
            if info.status == "exited" {
                head.push(Line::from(Span::styled(
                    format!(
                        " shell exited{} · close to send the result ",
                        info.exit.map(|c| format!(" ({c})")).unwrap_or_default()
                    ),
                    Style::default().fg(t.yellow).add_modifier(Modifier::BOLD),
                )));
            }
        } else {
            head.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", info.breadcrumb),
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("tty job · {}", info.status), t.muted()),
            ]));
            if let Some(cmd) = &info.command {
                head.push(Line::from(vec![
                    Span::styled(" $ ", t.muted()),
                    Span::styled(
                        truncate(first_line(cmd), w.saturating_sub(4)),
                        Style::default().fg(t.code_fg),
                    ),
                ]));
            }
            head.push(Line::from(Span::styled(
                truncate(
                    " AGENT-VISIBLE OUTPUT · the agent reads everything here ",
                    w,
                ),
                Style::default()
                    .fg(t.surface)
                    .bg(t.red)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        let head_h = (head.len() as u16).min(inner.height.saturating_sub(4));
        frame.render_widget(
            Paragraph::new(Text::from(head)).wrap(Wrap { trim: false }),
            Rect {
                y,
                height: head_h,
                ..inner
            },
        );
        y += head_h;
        // Action chips.
        let mut chips: Vec<(&str, TermAct)> = vec![];
        if info.user {
            if info.share_output {
                chips.push((" make private ", TermAct::Private));
            }
            if view.sel.is_some() {
                chips.push((" share selection ", TermAct::Share));
            }
            if info.staged > 0 {
                chips.push((" revoke excerpt ", TermAct::Revoke));
            }
            chips.push((" close… ", TermAct::Close));
        } else if info.alive() {
            chips.push((" kill ", TermAct::Kill));
        } else {
            chips.push((" remove tab ", TermAct::Dismiss));
        }
        let mut cx = inner.x + 1;
        for (label, act) in chips {
            let cw = label.width() as u16;
            if cx + cw > inner.right() {
                break;
            }
            let rect = Rect {
                x: cx,
                y,
                width: cw,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default().fg(t.ink).bg(t.code_bg),
                )),
                rect,
            );
            self.hits.add(rect, Target::Term(act));
            cx += cw + 1;
        }
        y += 1;
        // Rule with the focus state and its keys.
        let keys = match (self.terms.focus, self.terms.kbd_sel) {
            (TermFocus::Typing, _) if self.pending() > 0 => {
                " typing · ctrl+] panel keys · a prompt waits ".to_string()
            }
            (TermFocus::Typing, _) => " typing · ctrl+] panel keys ".to_string(),
            (TermFocus::Panel, true) => " ↑↓ extend · enter stage · esc cancel ".to_string(),
            (TermFocus::Panel, false) if info.user => {
                " i type · s select/share · p private · r revoke · c close · ←→ tabs · esc "
                    .to_string()
            }
            (TermFocus::Panel, false) => " i type · k kill · x remove · ←→ tabs · esc ".to_string(),
            (TermFocus::None, _) => " ctrl+] type here ".to_string(),
        };
        let keys = truncate(&keys, w.saturating_sub(2));
        let rule_style = Style::default().fg(if focused { t.accent } else { t.rule });
        let fill = w.saturating_sub(keys.width() + 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("─", rule_style),
                Span::styled(
                    keys,
                    if focused {
                        Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
                    } else {
                        t.faint()
                    },
                ),
                Span::styled("─".repeat(fill), rule_style),
            ])),
            Rect {
                y,
                height: 1,
                ..inner
            },
        );
        y += 1;
        // The screen, with a scrollbar column.
        let body = Rect {
            y,
            height: inner.bottom().saturating_sub(y),
            ..inner
        };
        self.terms.body = body;
        if body.height == 0 || body.width < 2 {
            return;
        }
        let screen = Rect {
            width: body.width - 1,
            ..body
        };
        let typing = self.terms.focus == TermFocus::Typing;
        let tx = self.terms.tx.clone();
        let view = &mut self.terms.views[self.terms.active];
        view.set_size(screen.height, screen.width);
        let want = (screen.width, screen.height);
        if view.pty_size != Some(want) && view.info.alive() {
            view.pty_size = Some(want);
            if let Some(tx) = tx {
                let _ = tx.send(TermCmd::Resize(view.info.id.clone(), want.0, want.1));
            }
        }
        view.unread = false;
        view.render(frame.buffer_mut(), screen, typing, t.select_bg);
        if view.lost {
            let note = " earlier output not kept ";
            let nx = screen.right().saturating_sub(note.width() as u16);
            frame.render_widget(
                Paragraph::new(Span::styled(note, t.faint())),
                Rect {
                    x: nx,
                    y: screen.y,
                    width: note.width() as u16,
                    height: 1,
                },
            );
        }
        let (len, back) = view.scroll_state();
        if back > 0 {
            let note = format!(" ↑ {back} rows back ");
            let nw = note.width() as u16;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    note,
                    Style::default().fg(t.surface).bg(t.accent),
                )),
                Rect {
                    x: screen.right().saturating_sub(nw),
                    y: screen.bottom() - 1,
                    width: nw.min(screen.width),
                    height: 1,
                },
            );
        }
        let cursor = if typing { view.cursor() } else { None };
        self.hits.add(screen, Target::Term(TermAct::Body));
        ui::scrollbar(
            frame.buffer_mut(),
            &mut self.hits,
            ScrollId::Terminal,
            body,
            len + screen.height as usize,
            screen.height as usize,
            len - back,
        );
        if let Some((cx, cy)) = cursor
            && matches!(self.overlay, Overlay::None)
            && cx < screen.width
            && cy < screen.height
        {
            frame.set_cursor_position((screen.x + cx, screen.y + cy));
        }
    }

    /// The status-line segment: (text, needs the user).
    pub(super) fn term_status(&self) -> Option<(String, bool)> {
        let n = self.terms.count();
        if n == 0 {
            return None;
        }
        let private = self.terms.open_private();
        Some(if private > 0 {
            (
                format!(
                    "⌨ {private} terminal{} waiting",
                    if private == 1 { "" } else { "s" }
                ),
                true,
            )
        } else {
            (format!("⌨ {n}"), false)
        })
    }

    /// The panel is typing-focused: the host cursor belongs to it.
    pub(super) fn term_typing(&self) -> bool {
        self.terms.focus == TermFocus::Typing && self.terms.visible()
    }
}

fn open_card(info: &TermInfo) -> Card {
    let prompt = info.prompt.clone().unwrap_or_default();
    let mut md = format!("**Prompt**\n\n{prompt}\n\n");
    if let Some(c) = info.command.as_deref().filter(|c| !c.is_empty()) {
        md.push_str(&format!(
            "**Proposed command** (typed, not run)\n\n`{c}`\n\n"
        ));
    }
    md.push_str(&format!(
        "**cwd** `{}` · **sharing** {} · **wait** {}",
        info.cwd,
        mode_text(info),
        if info.whenever {
            "whenever"
        } else {
            "blocking"
        }
    ));
    Card {
        key: format!("{}:open", info.id),
        icon: "⌨".into(),
        tone: if info.share_output {
            Tone::Warn
        } else {
            Tone::Accent
        },
        head: format!(
            "{} opened a private terminal · {} · {}",
            info.breadcrumb,
            truncate(first_line(&prompt), 60),
            mode_text(info)
        ),
        body: vec![CardPart::Md(md)],
    }
}
