//! One transcript entry: its content ([`Kind`]), how it is shown (folded,
//! scrolled, stamped, anchored), its rendered rows cached per width, and
//! the rendering of every kind but tool calls (see [`super::calls`]).
use super::calls::{full_output, render_pending, render_tool, unified_diff};
use super::{BoxInfo, Card, CardPart, Level, Pending, Rendered, SPINNER, Tone, Tool, boxed};
use crate::rows::{Btn, Meta, Row, button_row, plain, wrap_rows};
use crate::theme::theme;
use crate::wrap::{fmt_stamp, truncate};
use crate::{diff, markdown};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Visible rows of an expanded thinking block (Cockpit's `THINKING_VISIBLE`).
const THINKING_VISIBLE: usize = 20;

/// What an entry is.
pub enum Kind {
    User(String),
    /// A compact collapsible notice (scope and guidance updates, prune, local runs).
    Card(Box<Card>),
    Assistant(String),
    Reasoning(String),
    Tool(Box<Tool>),
    Pending(Box<Pending>),
    Notice(Level, String),
    /// Markdown produced by the TUI itself (/help).
    Info(String),
    /// A compaction in progress: the summary streams here.
    Compacting(String),
    /// "compacted" divider; expands to the summary.
    Compacted(String),
    /// An earlier, compacted context window, collapsed by default.
    Earlier(Vec<Entry>),
    /// Older context files still on disk: the agent and their
    /// numbers, oldest first; loaded one at a time when scrolled to.
    Older {
        agent: String,
        contexts: Vec<u32>,
        loading: bool,
    },
    /// A question answered through its fork: question id, summary.
    /// Its button reopens the fork's saved chat.
    ForkAnswer {
        question: String,
        summary: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    width: usize,
    rev: u64,
    expanded: bool,
    done: bool,
    turn_last: bool,
    anchored: bool,
    box_scroll: Option<usize>,
    show_ts: bool,
}

/// Which streamed text an append grows.
pub(super) enum Stream {
    Text,
    Reasoning,
    Compacting,
}

/// What an entry's copy buttons copy.
pub enum Clip {
    /// Agent markdown (one segment, or the whole turn from its copy row).
    Markdown(String),
    /// A user message, with its paste and attachment markers.
    User(String),
    /// A write/edit call as a unified diff.
    Diff(String),
}

/// One item of the transcript: its content and how it is shown (folded,
/// scrolled, stamped, anchored), with its rendered rows cached per width.
/// Any change to the content goes through a method that invalidates the
/// cache.
pub struct Entry {
    kind: Kind,
    expanded: bool,
    /// Assistant text: the segment is finished (code/table copy buttons appear).
    done: bool,
    /// Assistant text: the last segment of a finished turn. It carries the
    /// turn's copy row, which copies every segment of the turn; earlier
    /// segments show their own copy row only while hovered.
    turn_last: bool,
    /// User message: selected as an anchor (action row shown).
    anchored: bool,
    /// Inner box offset; None follows the tail (live) or sits at the top.
    box_scroll: Option<usize>,
    /// When a user or agent message was written (live: now; replay: the
    /// journal's `ts`), shown right-aligned on its first row.
    ts: Option<chrono::DateTime<chrono::Local>>,
    /// Whether `ts` is shown (user messages always; agent text on a
    /// turn's first segment, then only after a minute since the last shown).
    show_ts: bool,
    rev: u64,
    cache: Option<(Key, Rendered)>,
    /// The row count of a dropped cache: far off-screen entries keep
    /// only their height until they come back into view.
    height: Option<(Key, usize)>,
}

impl Entry {
    pub(super) fn new(kind: Kind, expanded: bool) -> Self {
        let ts = matches!(kind, Kind::User(_) | Kind::Assistant(_)).then(chrono::Local::now);
        Self {
            show_ts: matches!(kind, Kind::User(_)),
            ts,
            kind,
            expanded,
            done: false,
            turn_last: false,
            anchored: false,
            box_scroll: None,
            rev: 0,
            cache: None,
            height: None,
        }
    }
    fn key(&self, width: usize) -> Key {
        Key {
            width,
            rev: self.rev,
            expanded: self.expanded,
            done: self.done,
            turn_last: self.turn_last,
            anchored: self.anchored,
            box_scroll: self.box_scroll,
            show_ts: self.show_ts,
        }
    }
    /// The row count at `width`, without re-rendering an entry whose cache
    /// was dropped while nothing about it changed.
    pub fn height(&mut self, width: usize, tick: usize) -> usize {
        if self.cache.is_none()
            && !self.running()
            && let Some((k, h)) = &self.height
            && *k == self.key(width)
        {
            return *h;
        }
        self.render(width, tick).lines.len()
    }
    /// Drop the wrapped and highlighted rows of a rendered entry, keeping
    /// the height. An entry that is not rendered is left alone.
    pub fn trim(&mut self) {
        if self.cache.is_some() {
            self.evict();
        }
    }
    /// Drop this entry's rows and those of every earlier-context entry inside
    /// it, rendered or not (an uncached wrapper can hold cached children).
    fn evict(&mut self) {
        if let Some((k, r)) = self.cache.take() {
            self.height = Some((k, r.lines.len()));
        }
        if let Kind::Earlier(children) = &mut self.kind {
            children.iter_mut().for_each(Entry::evict);
        }
    }
    fn touch(&mut self) {
        self.rev += 1;
    }
    /// Whether a header click toggles this entry.
    pub fn selectable(&self) -> bool {
        match &self.kind {
            Kind::Reasoning(_) | Kind::Compacted(_) | Kind::Earlier(_) => true,
            Kind::Tool(t) => t.output.is_some() || has_body(t) || t.review.is_some(),
            Kind::Notice(_, text) => text.lines().count() > NOTICE_LINES,
            Kind::Card(c) => !c.body.is_empty(),
            _ => false,
        }
    }
    /// Expand or collapse a selectable entry; false when it has nothing to fold.
    pub fn toggle(&mut self) -> bool {
        if !self.selectable() {
            return false;
        }
        self.expanded = !self.expanded;
        true
    }
    pub(super) fn expand(&mut self) {
        self.expanded = true;
    }
    /// Open a collapsed earlier context; false when this is not one or it
    /// is open already.
    pub(super) fn open_earlier(&mut self) -> bool {
        if matches!(self.kind, Kind::Earlier(_)) && !self.expanded {
            self.expanded = true;
            return true;
        }
        false
    }
    pub(super) fn collapse(&mut self) {
        self.expanded = false;
    }
    /// Reasoning blocks follow the "show reasoning" switch.
    pub(super) fn reveal_reasoning(&mut self, show: bool) {
        if matches!(self.kind, Kind::Reasoning(_)) {
            self.expanded = show;
        }
    }
    fn running(&self) -> bool {
        matches!(&self.kind, Kind::Tool(t) if t.output.is_none() || t.review.as_ref().is_some_and(|r| r.result.is_none()))
            || matches!(&self.kind, Kind::Pending(p) if p.abandoned.is_none())
            || matches!(self.kind, Kind::Compacting(_))
            || matches!(self.kind, Kind::Older { loading: true, .. })
    }
    /// Compact one-line rows: no blank separator between neighbours. The
    /// state never depends on `expanded`, so toggling never moves a header.
    fn is_row(&self) -> bool {
        matches!(
            self.kind,
            Kind::Tool(_)
                | Kind::Pending(_)
                | Kind::Notice(..)
                | Kind::Card(_)
                | Kind::Reasoning(_)
                | Kind::ForkAnswer { .. }
        )
    }
    /// Whether a blank separator row sits between `prev` and this entry.
    pub(super) fn separated_from(&self, prev: &Entry) -> bool {
        !(self.is_row() && prev.is_row())
            // Earlier contexts stack as one list of headers.
            && !(self.is_history() && prev.is_history())
    }
    /// An earlier context or the "older contexts on disk" row: the headers
    /// that stack at the top of a transcript.
    pub(super) fn is_history(&self) -> bool {
        matches!(self.kind, Kind::Earlier(_) | Kind::Older { .. })
    }
    pub fn render(&mut self, width: usize, tick: usize) -> &Rendered {
        let key = self.key(width);
        // Running rows animate a spinner, so they are never cached.
        let fresh = !self.running() && matches!(&self.cache, Some((k, _)) if *k == key);
        if !fresh {
            // An expanded earlier context renders from its children's caches.
            if self.expanded
                && let Kind::Earlier(children) = &mut self.kind
            {
                for c in children.iter_mut() {
                    c.render(width.saturating_sub(4), tick);
                }
            }
            let (rows, boxinfo) = self.draw_stamped(width, tick);
            let (lines, meta) = rows.into_iter().unzip();
            self.cache = Some((
                key,
                Rendered {
                    lines,
                    meta,
                    boxinfo,
                },
            ));
        }
        &self.cache.as_ref().unwrap().1
    }
    /// The rows (by index) whose text contains `query` (lowercase). A
    /// collapsed earlier context is searched as if open.
    pub(super) fn matching_lines(&mut self, query: &str, width: usize, tick: usize) -> Vec<usize> {
        let fold = matches!(self.kind, Kind::Earlier(_)) && !self.expanded;
        if fold {
            self.expanded = true;
        }
        let r = self.render(width, tick);
        let mut found = vec![];
        for (j, line) in r.lines.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if text.to_lowercase().contains(query) {
                found.push(j);
            }
        }
        if fold {
            self.expanded = false;
        }
        found
    }
    /// Scroll the inner box by `delta` rows; false when it cannot move that
    /// way (the outer transcript should scroll instead).
    pub fn scroll_box(&mut self, delta: isize) -> bool {
        let Some(info) = self.boxinfo() else {
            return false;
        };
        let next = (info.offset as isize + delta).clamp(0, info.max as isize) as usize;
        if next == info.offset {
            return false;
        }
        self.box_scroll = if info.follow && next >= info.max {
            None
        } else {
            Some(next)
        };
        true
    }
    /// Put the inner box at `offset`; at the `end` a following box goes
    /// back to tracking its tail.
    pub fn scroll_box_to(&mut self, offset: usize, end: bool) {
        let follow = self.boxinfo().is_some_and(|b| b.follow);
        self.box_scroll = if follow && end { None } else { Some(offset) };
    }
    fn boxinfo(&self) -> Option<BoxInfo> {
        self.cache.as_ref().and_then(|(_, r)| r.boxinfo)
    }

    // ---- content ----------------------------------------------------

    pub(super) fn is_user(&self) -> bool {
        matches!(self.kind, Kind::User(_))
    }
    /// The text of a user message.
    pub fn user_text(&self) -> Option<&str> {
        match &self.kind {
            Kind::User(t) => Some(t),
            _ => None,
        }
    }
    pub(super) fn is_assistant(&self) -> bool {
        matches!(self.kind, Kind::Assistant(_))
    }
    /// The markdown of an agent text segment.
    pub(super) fn assistant_text(&self) -> Option<&str> {
        match &self.kind {
            Kind::Assistant(m) => Some(m),
            _ => None,
        }
    }
    /// A finished agent segment that is not its turn's last: it shows its
    /// copy row only while hovered.
    pub fn shows_hover_copy(&self) -> bool {
        self.is_assistant() && self.done && !self.turn_last
    }
    /// The call of a tool row.
    pub(super) fn tool(&self) -> Option<&Tool> {
        match &self.kind {
            Kind::Tool(t) => Some(t),
            _ => None,
        }
    }
    /// Change the call of a tool row; `edit` says whether it changed
    /// anything. False for other rows.
    pub(super) fn update_tool(&mut self, edit: impl FnOnce(&mut Tool) -> bool) -> bool {
        let Kind::Tool(t) = &mut self.kind else {
            return false;
        };
        let changed = edit(t);
        if changed {
            self.touch();
        }
        changed
    }
    /// The call of a pending (still being written) row.
    pub(super) fn pending(&self) -> Option<&Pending> {
        match &self.kind {
            Kind::Pending(p) => Some(p),
            _ => None,
        }
    }
    /// [`Entry::update_tool`] for a pending row.
    pub(super) fn update_pending(&mut self, edit: impl FnOnce(&mut Pending) -> bool) -> bool {
        let Kind::Pending(p) = &mut self.kind else {
            return false;
        };
        let changed = edit(p);
        if changed {
            self.touch();
        }
        changed
    }
    /// [`Entry::update_tool`] for a notice row.
    pub(super) fn update_notice(&mut self, edit: impl FnOnce(Level, &mut String) -> bool) -> bool {
        let Kind::Notice(level, text) = &mut self.kind else {
            return false;
        };
        let changed = edit(*level, text);
        if changed {
            self.touch();
        }
        changed
    }
    /// Grow streamed text of the kind `into`; false when this entry is
    /// not that stream.
    pub(super) fn append(&mut self, into: Stream, delta: &str) -> bool {
        let text = match (&mut self.kind, into) {
            // A finished reply takes no more text: the next one is its own
            // paragraph.
            (Kind::Assistant(t), Stream::Text) if !self.done => t,
            (Kind::Reasoning(t), Stream::Reasoning) | (Kind::Compacting(t), Stream::Compacting) => {
                t
            }
            _ => return false,
        };
        text.push_str(delta);
        self.touch();
        true
    }
    /// The content changes in place (a pending call became its row, a
    /// card was updated); the fold state is kept.
    pub(super) fn replace(&mut self, kind: Kind) {
        self.kind = kind;
        self.box_scroll = None;
        self.touch();
    }
    pub(super) fn is_card(&self, key: &str) -> bool {
        matches!(&self.kind, Kind::Card(c) if c.key == key)
    }
    pub(super) fn is_compacting(&self) -> bool {
        matches!(self.kind, Kind::Compacting(_))
    }
    /// Rows a fold into an earlier context drops (live, unfinished state).
    pub(super) fn folds_away(&self) -> bool {
        matches!(self.kind, Kind::Compacting(_) | Kind::Pending(_))
    }
    /// Model output of the current attempt that a stream reset discards.
    pub(super) fn is_streamed(&self) -> bool {
        matches!(self.kind, Kind::Assistant(_) | Kind::Reasoning(_))
    }
    /// Whether the session has anything a resume would bring back.
    pub(super) fn is_conversation(&self) -> bool {
        matches!(
            self.kind,
            Kind::User(_) | Kind::Assistant(_) | Kind::Earlier(_) | Kind::Older { .. }
        )
    }
    pub(super) fn is_compacted(&self, summary: &str) -> bool {
        matches!(&self.kind, Kind::Compacted(s) if s == summary)
    }
    /// A `task` row that continues an existing child.
    pub(super) fn continues_task(&self) -> bool {
        self.tool().is_some_and(|t| t.args["continue"].is_string())
    }
    /// The child a `task` row is linked to.
    pub fn task_child(&self) -> Option<&str> {
        self.tool()?.child.as_ref().map(|c| c.id.as_str())
    }
    /// The question and summary of an answer given through a fork.
    pub fn fork_answer(&self) -> Option<(&str, &str)> {
        match &self.kind {
            Kind::ForkAnswer { question, summary } => Some((question, summary)),
            _ => None,
        }
    }
    /// The file holding a tool result's full output, when it was cut.
    pub fn output_file(&self) -> Option<std::path::PathBuf> {
        let out = self.tool()?.output.as_deref()?;
        full_output(out).map(|(_, _, p)| p)
    }
    /// What this entry's own copy buttons copy (a turn's copy row copies
    /// the whole turn: see [`Entry::ends_turn`]).
    pub(super) fn clip(&self) -> Option<Clip> {
        match &self.kind {
            Kind::Assistant(m) => Some(Clip::Markdown(m.clone())),
            Kind::User(t) => Some(Clip::User(t.clone())),
            Kind::Tool(t) => unified_diff(t).map(Clip::Diff),
            _ => None,
        }
    }
    /// The last agent segment of a finished turn (it carries the turn's
    /// copy row).
    pub(super) fn ends_turn(&self) -> bool {
        self.is_assistant() && self.turn_last
    }
    /// The actions an entry offers (buttons, menu items, keyboard focus).
    pub fn buttons(&self) -> Vec<Btn> {
        match &self.kind {
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
            Kind::Tool(t) if t.output.as_deref().and_then(full_output).is_some() => {
                vec![Btn::Output]
            }
            _ => vec![],
        }
    }

    // ---- turns and times --------------------------------------------

    /// The previous agent segment is finished once anything follows it.
    pub(super) fn seal(&mut self) {
        if matches!(self.kind, Kind::Assistant(_)) && !self.done {
            self.done = true;
        }
    }
    /// Mark every agent segment finished (turn end, replay) and the last
    /// segment of each turn as carrying the turn's copy row; settled retry
    /// rows read as history: "retried 3× · reason".
    pub(super) fn seal_all(entries: &mut [Entry]) {
        for e in entries.iter_mut() {
            e.update_notice(|level, t| {
                if level != Level::Warn {
                    return false;
                }
                let Some(rest) = t.strip_prefix("reconnecting · attempt ") else {
                    return false;
                };
                let n = rest.split(' ').next().unwrap_or("1").to_string();
                *t = format!(
                    "network dropped · {n} reconnect attempt{}",
                    if n == "1" { "" } else { "s" }
                );
                true
            });
            e.update_notice(|level, t| {
                if level != Level::Warn {
                    return false;
                }
                let Some((n, after)) = t
                    .strip_prefix("retry ")
                    .and_then(|rest| rest.split_once(" in "))
                else {
                    return false;
                };
                if !n.chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                let reason = after.split_once(" · ").map(|(_, r)| r);
                *t = match reason {
                    Some(r) => format!("retried {n}× · {r}"),
                    None => format!("retried {n}×"),
                };
                true
            });
        }
        let mut last: Option<usize> = None;
        for i in 0..entries.len() {
            match entries[i].kind {
                Kind::Assistant(_) => {
                    entries[i].done = true;
                    entries[i].turn_last = false;
                    last = Some(i);
                }
                Kind::User(_) => {
                    if let Some(l) = last.take() {
                        entries[l].turn_last = true;
                    }
                }
                _ => {}
            }
        }
        if let Some(l) = last {
            entries[l].turn_last = true;
        }
    }
    /// Agent text shows its time on the first segment of a turn, then
    /// only when a minute has passed since the last time shown (`before`
    /// are the entries above this one). A turn starts at a user message,
    /// an agent or tree message, a question answer or a compaction. It
    /// depends only on the entries, so a replay stamps exactly as the live
    /// view did.
    pub(super) fn decide_stamp(&mut self, before: &[Entry]) {
        let show = match self.kind {
            Kind::User(_) => true,
            Kind::Assistant(_) => {
                let Some(ts) = self.ts else { return };
                let mut show = true;
                for prev in before.iter().rev() {
                    match &prev.kind {
                        Kind::User(_)
                        | Kind::Compacted(_)
                        | Kind::Earlier(_)
                        | Kind::Notice(Level::Message | Level::Answer, _) => break,
                        Kind::Assistant(_) if prev.show_ts => {
                            show = prev.ts.is_none_or(|p| (ts - p).num_seconds() >= 60);
                            break;
                        }
                        _ => {}
                    }
                }
                show
            }
            _ => false,
        };
        if self.show_ts != show {
            self.show_ts = show;
        }
    }
    /// Give the entry its journal time (replay).
    pub(super) fn set_time(&mut self, t: chrono::DateTime<chrono::Local>) {
        self.ts = Some(t);
        self.touch();
    }
    /// Anchor a user message (`on` for any other entry does nothing).
    pub(super) fn set_anchored(&mut self, on: bool) {
        let on = on && self.is_user();
        if self.anchored != on {
            self.anchored = on;
        }
    }
    pub(super) fn is_anchored(&self) -> bool {
        self.anchored
    }

    // ---- older contexts on disk -------------------------------------

    /// Older context `n` was loaded: the "on disk" row forgets it. Some
    /// with whether none are left; None when this is not that row.
    pub(super) fn older_loaded(&mut self, n: u32) -> Option<bool> {
        let Kind::Older {
            contexts, loading, ..
        } = &mut self.kind
        else {
            return None;
        };
        contexts.retain(|c| *c != n);
        *loading = false;
        let empty = contexts.is_empty();
        self.touch();
        Some(empty)
    }
    /// The next older context to load, marking the row as loading.
    pub(super) fn take_older(&mut self) -> Option<(String, u32)> {
        match &mut self.kind {
            Kind::Older {
                agent,
                contexts,
                loading,
            } if !*loading => {
                let n = *contexts.last()?;
                *loading = true;
                self.rev += 1;
                Some((agent.clone(), n))
            }
            _ => None,
        }
    }
}

fn has_body(t: &Tool) -> bool {
    matches!(t.name.as_str(), "write" | "edit")
}

impl Entry {
    fn draw(&self, width: usize, tick: usize) -> (Vec<Row>, Option<BoxInfo>) {
        let t = theme();
        let width = width.max(8);
        let mut boxinfo = None;
        let rows = match &self.kind {
            Kind::User(text) => {
                let bar = Span::styled("▌ ", Style::default().fg(t.accent));
                // Paste blocks show as one chip row each; a click opens
                // them. Attachment chips stay inline as labels and are
                // repeated as clickable chips in a tray under the text.
                let mut body: Vec<Row> = vec![];
                let mut n = 0usize;
                let mut cur: Vec<Span<'static>> = vec![];
                let mut tray: Vec<(String, bool)> = vec![];
                let segs = crate::paste::parse(text);
                let last = segs.len().saturating_sub(1);
                let line = |cur: &mut Vec<Span<'static>>| {
                    (Line::from(std::mem::take(cur)), Meta::gutter(0))
                };
                for (si, seg) in segs.iter().enumerate() {
                    match seg {
                        crate::paste::Seg::Text(part) => {
                            let mut part = *part;
                            let paste_at = |i: usize| {
                                matches!(segs.get(i), Some(crate::paste::Seg::Paste { .. }))
                            };
                            if si > 0 && paste_at(si - 1) {
                                // Text after a paste chip starts its own row.
                                part = part.strip_prefix('\n').unwrap_or(part);
                                part = part.trim_start_matches(' ');
                            }
                            if paste_at(si + 1) || si == last {
                                part = part.strip_suffix('\n').unwrap_or(part);
                            }
                            for (i, piece) in part.split('\n').enumerate() {
                                if i > 0 {
                                    body.push(line(&mut cur));
                                }
                                if !piece.is_empty() {
                                    cur.push(Span::styled(piece.to_string(), t.bold()));
                                }
                            }
                        }
                        crate::paste::Seg::Paste { body: full, .. } => {
                            if !cur.is_empty() {
                                body.push(line(&mut cur));
                            }
                            let label = crate::paste::label(n as u32 + 1, full);
                            let w = label.width() as u16;
                            body.push((
                                Line::from(Span::styled(label, Style::default().fg(t.link))),
                                Meta {
                                    skip: true,
                                    buttons: vec![(0, w, Btn::Paste(n))],
                                    ..Meta::default()
                                },
                            ));
                            n += 1;
                        }
                        crate::paste::Seg::Attach { kind, path, .. } => {
                            let label = crate::attach::label(tray.len() as u32 + 1, kind, path);
                            cur.push(Span::styled(label.clone(), Style::default().fg(t.link)));
                            tray.push((label, !std::path::Path::new(path).exists()));
                        }
                    }
                }
                if !cur.is_empty() {
                    body.push(line(&mut cur));
                }
                // The tray: chips packed into rows; a click opens the file.
                let room = width.saturating_sub(2).max(10);
                let mut spans: Vec<Span<'static>> = vec![];
                let mut buttons = vec![];
                let mut x = 0usize;
                for (i, (label, missing)) in tray.iter().enumerate() {
                    let label = if *missing {
                        format!("{label} (missing)")
                    } else {
                        label.clone()
                    };
                    let w = label.width();
                    if x > 0 && x + 2 + w > room {
                        body.push((
                            Line::from(std::mem::take(&mut spans)),
                            Meta {
                                skip: true,
                                buttons: std::mem::take(&mut buttons),
                                ..Meta::default()
                            },
                        ));
                        x = 0;
                    }
                    if x > 0 {
                        spans.push(Span::raw("  "));
                        x += 2;
                    }
                    if !missing {
                        buttons.push((x as u16, (x + w) as u16, Btn::Attachment(i)));
                    }
                    spans.push(Span::styled(
                        label,
                        if *missing {
                            t.faint()
                        } else {
                            Style::default().fg(t.link).bg(t.surface)
                        },
                    ));
                    x += w;
                }
                if !spans.is_empty() {
                    body.push((
                        Line::from(spans),
                        Meta {
                            skip: true,
                            buttons,
                            ..Meta::default()
                        },
                    ));
                }
                let mut rows = wrap_rows(
                    body,
                    width,
                    std::slice::from_ref(&bar),
                    std::slice::from_ref(&bar),
                );
                if self.anchored {
                    rows.push(button_row(
                        "  ↳ ",
                        &[Btn::Fork, Btn::CopyUser],
                        Style::default().fg(t.dim),
                    ));
                }
                rows
            }
            Kind::Assistant(text) | Kind::Info(text) => {
                let assistant = matches!(self.kind, Kind::Assistant(_));
                let buttons = assistant && self.done;
                let turn_row = assistant && self.turn_last;
                let pad = [Span::raw("  ")];
                let md = markdown::render_md(text, width.saturating_sub(2), buttons);
                let rows = md
                    .rows
                    .into_iter()
                    .map(|(l, m)| {
                        if l.style == Style::default() {
                            (l.style(t.text()), m)
                        } else {
                            (l, m)
                        }
                    })
                    .collect();
                let mut rows = wrap_rows(rows, width, &pad, &pad);
                if turn_row {
                    rows.push(button_row(
                        "  ",
                        &[Btn::CopyText, Btn::CopyRich, Btn::CopyMarkdown],
                        Style::default().fg(t.dim),
                    ));
                }
                rows
            }
            Kind::Reasoning(text) => {
                let words = text.split_whitespace().count();
                let style = Style::default().fg(t.dim).add_modifier(Modifier::ITALIC);
                if !self.expanded {
                    let preview = text
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("");
                    let head = format!("  ▸ thinking · {words} words  ");
                    let room = width.saturating_sub(head.chars().count() + 1);
                    vec![(
                        Line::from(vec![
                            Span::styled(head, Style::default().fg(t.dim)),
                            Span::styled(truncate(preview.trim(), room), style),
                        ]),
                        Meta {
                            skip: true,
                            header: true,
                            ..Meta::default()
                        },
                    )]
                } else {
                    let mut out = vec![(
                        Line::from(Span::styled(
                            format!("  ▾ thinking · {words} words"),
                            Style::default().fg(t.dim),
                        )),
                        Meta {
                            skip: true,
                            header: true,
                            ..Meta::default()
                        },
                    )];
                    let lines = text
                        .trim_end()
                        .lines()
                        .map(|l| Line::from(Span::styled(l.to_string(), style)))
                        .collect();
                    let gutter = Span::styled("  ┊ ", Style::default().fg(t.dim));
                    // The last column is reserved for the box scrollbar.
                    let body = wrap_rows(
                        plain(lines, 0),
                        width.saturating_sub(1),
                        std::slice::from_ref(&gutter),
                        std::slice::from_ref(&gutter),
                    );
                    boxinfo = boxed(
                        &mut out,
                        body,
                        THINKING_VISIBLE,
                        self.box_scroll,
                        true,
                        &gutter,
                    );
                    out
                }
            }
            Kind::Notice(level, text) => {
                let (icon, color) = match level {
                    Level::Info => ("·", t.fog),
                    // ↻ for retries, ! for every other warning.
                    Level::Warn
                        if text.starts_with("retr")
                            || text.starts_with("reconnect")
                            || text.starts_with("network dropped") =>
                    {
                        ("↻", t.yellow)
                    }
                    Level::Warn => ("!", t.yellow),
                    Level::Error => ("✗", t.red),
                    Level::Message => ("✉", t.link),
                    Level::Job => ("⚙", t.fog),
                    Level::Answer => ("↳", t.green),
                };
                // Long notices (a child's report) fold to their first lines.
                let all: Vec<&str> = text.lines().collect();
                let fold = all.len() > NOTICE_LINES && !self.expanded;
                let shown = if fold { NOTICE_LINES - 1 } else { all.len() };
                let icon_span = Span::styled(
                    format!("  {icon} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                );
                let indent = Span::raw("    ");
                let mut rows = vec![];
                for (i, l) in all.iter().take(shown).enumerate() {
                    let base = Style::default().fg(color);
                    let line = if matches!(level, Level::Job) {
                        Line::from(markdown::inline_truncated(l, base, width.saturating_sub(4)))
                    } else {
                        Line::from(Span::styled(l.to_string(), base))
                    };
                    // The icon marks the notice once, not every line.
                    let first = if i == 0 { &icon_span } else { &indent };
                    let mut r = wrap_rows(
                        plain(vec![line], 0),
                        width,
                        std::slice::from_ref(first),
                        std::slice::from_ref(&indent),
                    );
                    if i == 0 && all.len() > NOTICE_LINES {
                        r[0].1.header = true;
                    }
                    rows.extend(r);
                }
                if all.len() > NOTICE_LINES {
                    let label = if fold {
                        format!("… {} more lines (click to expand)", all.len() - shown)
                    } else {
                        "(click to collapse)".to_string()
                    };
                    rows.push((
                        Line::from(vec![indent.clone(), Span::styled(label, t.faint())]),
                        Meta {
                            skip: true,
                            header: true,
                            ..Meta::default()
                        },
                    ));
                }
                // Errors (live, or persisted turn errors replayed on resume)
                // stand out on a tinted band across the row.
                if matches!(level, Level::Error) {
                    for (l, _) in &mut rows {
                        let w: usize = l.spans.iter().map(|s| s.content.width()).sum();
                        if w < width {
                            l.spans.push(Span::raw(" ".repeat(width - w)));
                        }
                        *l = l.clone().style(Style::default().bg(t.diff_del_bg));
                    }
                }
                rows
            }
            Kind::Card(card) => render_card(card, self.expanded, width),
            Kind::ForkAnswer { summary, .. } => {
                let head = "  ↳ answered via fork: ";
                let btn = Btn::OpenFork.label();
                let room = width.saturating_sub(head.width() + btn.width() + 2);
                let text = truncate(&summary.replace('\n', " · "), room);
                let x = head.width() + text.width() + 2;
                let mut meta = Meta::default();
                meta.buttons
                    .push((x as u16, (x + btn.width()) as u16, Btn::OpenFork));
                vec![(
                    Line::from(vec![
                        Span::styled(head, Style::default().fg(t.green)),
                        Span::styled(text, Style::default().fg(t.green)),
                        Span::raw("  "),
                        Span::styled(btn, Style::default().fg(t.dim)),
                    ]),
                    meta,
                )]
            }
            Kind::Tool(tool) => {
                let (rows, b) = render_tool(tool, self.expanded, self.box_scroll, width, tick);
                boxinfo = b;
                rows
            }
            Kind::Pending(p) => {
                let (rows, b) = render_pending(p, self.box_scroll, width, tick);
                boxinfo = b;
                rows
            }
            Kind::Compacting(text) => {
                let words = text.split_whitespace().count();
                vec![(
                    Line::from(Span::styled(
                        format!(
                            "  {} compacting context · {words} words of summary",
                            SPINNER[tick % SPINNER.len()]
                        ),
                        Style::default().fg(t.yellow),
                    )),
                    Meta::skip(),
                )]
            }
            Kind::Compacted(summary) => {
                let words = summary.split_whitespace().count();
                let marker = if self.expanded { "▾" } else { "▸" };
                let label = format!(" {marker} compacted · summary {words} words ");
                let side = width.saturating_sub(label.chars().count()) / 2;
                let mut out = vec![(
                    Line::from(vec![
                        Span::styled("─".repeat(side), Style::default().fg(t.rule)),
                        Span::styled(label, Style::default().fg(t.accent)),
                        Span::styled("─".repeat(side), Style::default().fg(t.rule)),
                    ]),
                    Meta {
                        skip: true,
                        header: true,
                        ..Meta::default()
                    },
                )];
                if self.expanded {
                    let gutter = [Span::styled("  ┊ ", Style::default().fg(t.dim))];
                    let rows = markdown::render_md(summary, width.saturating_sub(4), false)
                        .rows
                        .into_iter()
                        .map(|(l, m)| (l.style(Style::default().fg(t.fog)), m))
                        .collect();
                    out.extend(wrap_rows(rows, width, &gutter, &gutter));
                }
                out
            }
            Kind::Older {
                contexts, loading, ..
            } => {
                let n = contexts.len();
                let text = if *loading {
                    format!(
                        "  {} loading earlier context…",
                        SPINNER[tick % SPINNER.len()]
                    )
                } else {
                    format!(
                        "  ⋯ {n} earlier context{} on disk · scroll up to load",
                        if n == 1 { "" } else { "s" }
                    )
                };
                vec![(
                    Line::from(Span::styled(text, Style::default().fg(t.dim))),
                    Meta {
                        skip: true,
                        ..Meta::default()
                    },
                )]
            }
            Kind::Earlier(entries) => {
                let users = entries
                    .iter()
                    .filter(|e| matches!(e.kind, Kind::User(_)))
                    .count();
                let marker = if self.expanded { "▾" } else { "▸" };
                let mut out = vec![(
                    Line::from(Span::styled(
                        format!(
                            "  {marker} earlier context (compacted) · {users} message{}, {} entr{}",
                            if users == 1 { "" } else { "s" },
                            entries.len(),
                            if entries.len() == 1 { "y" } else { "ies" }
                        ),
                        Style::default().fg(t.dim),
                    )),
                    Meta {
                        skip: true,
                        header: true,
                        ..Meta::default()
                    },
                )];
                if self.expanded {
                    let gutter = Span::styled("  ┃ ", Style::default().fg(t.rule));
                    for (i, e) in entries.iter().enumerate() {
                        if i > 0 && !(e.is_row() && entries[i - 1].is_row()) {
                            out.push((Line::from(gutter.clone()), Meta::gutter(4)));
                        }
                        for (l, m) in e.rows(width.saturating_sub(4), tick) {
                            let mut spans = vec![gutter.clone()];
                            spans.extend(l.spans);
                            out.push((
                                Line::from(spans).style(l.style),
                                Meta {
                                    gutter: m.gutter + 4,
                                    buttons: vec![],
                                    header: false,
                                    boxed: false,
                                    ..m
                                },
                            ));
                        }
                    }
                }
                out
            }
        };
        (rows, boxinfo)
    }

    /// A child's rows, from its own cache when it has one at this width.
    fn rows(&self, width: usize, tick: usize) -> Vec<Row> {
        match &self.cache {
            Some((k, r)) if *k == self.key(width) => r
                .lines
                .iter()
                .cloned()
                .zip(r.meta.iter().cloned())
                .collect(),
            _ => self.draw_stamped(width, tick).0,
        }
    }

    /// [`Entry::draw`] plus the message time right-aligned on the first row. The
    /// first row keeps its full width when the time fits beside it; otherwise
    /// the message wraps narrower so the time never covers text.
    fn draw_stamped(&self, width: usize, tick: usize) -> (Vec<Row>, Option<BoxInfo>) {
        let Some(ts) = self.ts.filter(|_| self.show_ts && width >= 40) else {
            return self.draw(width, tick);
        };
        let label = fmt_stamp(&ts);
        // Two columns of air before the time, one after it.
        let need = label.width() + 3;
        let first_w = |rows: &[Row]| -> usize {
            rows.first()
                .map(|(l, _)| l.spans.iter().map(|s| s.content.width()).sum())
                .unwrap_or(0)
        };
        let (mut rows, boxinfo) = self.draw(width, tick);
        if rows.is_empty() {
            return (rows, boxinfo);
        }
        if first_w(&rows) + need > width {
            let (narrow, b) = self.draw(width - need, tick);
            if narrow.is_empty() {
                return (rows, boxinfo);
            }
            rows = narrow;
            let _ = b;
        }
        let pad = width.saturating_sub(first_w(&rows) + label.width() + 1);
        let t = theme();
        rows[0].1.text_end = Some(first_w(&rows) as u16);
        let first = &mut rows[0].0;
        first.spans.push(Span::raw(" ".repeat(pad)));
        first
            .spans
            .push(Span::styled(label, Style::default().fg(t.dim)));
        (rows, boxinfo)
    }
}

/// Notices longer than this fold to their first lines until clicked.
const NOTICE_LINES: usize = 6;

/// A card: `  icon head ▸`, one line while collapsed; expanded, the head
/// wraps and the body follows under a dim gutter.
fn render_card(card: &Card, expanded: bool, width: usize) -> Vec<Row> {
    let t = theme();
    let color = match card.tone {
        Tone::Info => t.fog,
        Tone::Warn => t.yellow,
        Tone::Ok => t.green,
        Tone::Err => t.red,
        Tone::Accent => t.link,
    };
    let icon = Span::styled(
        format!("  {} ", card.icon),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
    let marker = if card.body.is_empty() {
        String::new()
    } else if expanded {
        " ▾".into()
    } else {
        " ▸".into()
    };
    let head_style = Style::default().fg(color);
    let mut out: Vec<Row> = if expanded {
        let line = Line::from(vec![
            Span::styled(card.head.clone(), head_style),
            Span::styled(marker, Style::default().fg(t.dim)),
        ]);
        wrap_rows(
            plain(vec![line], 0),
            width,
            std::slice::from_ref(&icon),
            &[Span::raw("    ")],
        )
    } else {
        let room = width.saturating_sub(icon.content.width() + marker.width());
        vec![(
            Line::from(vec![
                icon.clone(),
                Span::styled(truncate(&card.head, room), head_style),
                Span::styled(marker, Style::default().fg(t.dim)),
            ]),
            Meta::gutter(icon.content.width()),
        )]
    };
    if !card.body.is_empty() {
        out[0].1.header = true;
        out[0].1.skip = true;
    }
    if !expanded {
        return out;
    }
    let gutter = Span::styled("    ┊ ", Style::default().fg(t.dim));
    let inner = width.saturating_sub(gutter.content.width());
    for (i, part) in card.body.iter().enumerate() {
        if i > 0 {
            out.push((
                Line::from(gutter.clone()),
                Meta::gutter(gutter.content.width()),
            ));
        }
        match part {
            CardPart::Md(md) => {
                let rows = markdown::render_md(md, inner, false).rows;
                out.extend(wrap_rows(
                    rows,
                    width,
                    std::slice::from_ref(&gutter),
                    std::slice::from_ref(&gutter),
                ));
            }
            CardPart::Diff {
                path,
                old,
                new,
                write,
            } => {
                let d = diff::render_file(path, old, new, write.then_some((1, 1)));
                let hang = [gutter.clone(), Span::raw(" ".repeat(d.indent))];
                out.extend(wrap_rows(
                    plain(d.lines, d.indent),
                    width,
                    std::slice::from_ref(&gutter),
                    &hang,
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Entry {
        pub(crate) fn kind(&self) -> &Kind {
            &self.kind
        }
    }
}
