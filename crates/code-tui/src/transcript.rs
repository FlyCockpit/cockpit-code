//! Transcript model and rendering: user messages, assistant markdown
//! segments, reasoning, compact tool rows (with diffs and checklists),
//! pending (still being written) tool calls, notices. Rendered rows carry
//! [`Meta`] for selection, copy buttons and toggles, and are cached per
//! entry and width. Long thinking and long tool results render inside an
//! inner scroll box with its own offset.
use crate::engine::{FileEdit, TodoItem, ToolCall, ToolResultData, ToolResultDetails, todo_items};
use crate::rows::{Btn, Meta, Row, button_row, plain, wrap_rows};
use crate::theme::theme;
use crate::wrap::truncate;
use crate::{diff, markdown};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const COLLAPSED_DIFF_ROWS: usize = 12;
const COLLAPSED_TODO_ROWS: usize = 20;
/// Rows kept from a tool result before the box (the box scrolls within them).
const MAX_OUTPUT_LINES: usize = 5000;
/// Visible rows of an expanded thinking block (Cockpit's `THINKING_VISIBLE`).
pub const THINKING_VISIBLE: usize = 20;
/// Visible rows of an expanded tool result (Cockpit's `TOOLCALL_RESULT_VISIBLE`).
pub const RESULT_VISIBLE: usize = 20;
/// Visible rows of the live preview of a write/edit being written.
pub const PREVIEW_VISIBLE: usize = 8;
/// How long an abandoned pending call stays on screen.
pub const ABANDONED_FOR: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
    /// A tree message (child → parent, or a steering message).
    Message,
    /// A background job finished.
    Job,
    /// The user answered a question (D62): a compact row, not a message.
    Answer,
}

/// The subagent a `task` tool row started or continued.
#[derive(Clone)]
pub struct ChildLink {
    pub id: String,
    pub label: String,
    pub status: String,
    /// `task {continue}`: a follow-up to a finished child.
    pub follow_up: bool,
}

/// A sidecar safety review of a call (D46).
#[derive(Clone)]
pub struct Review {
    pub model: String,
    /// None while the review runs; then (safe, verdict, reason).
    pub result: Option<(bool, String, String)>,
}

pub struct Tool {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub output: Option<String>,
    pub is_error: bool,
    pub started: Instant,
    pub duration: Option<Duration>,
    /// Restored from a session file: no timing.
    pub replayed: bool,
    /// For `task` rows: the child agent, once known.
    pub child: Option<ChildLink>,
    pub review: Option<Review>,
    /// `script` rows: inner host calls, oldest first (engine-m records).
    pub inner: Vec<Inner>,
    /// Successful write/edit results: the hunks with their line numbers.
    pub edits: Vec<FileEdit>,
    /// A steer or interrupt moved this foreground call to the background
    /// (D72/D74): the engine's reason.
    pub moved: Option<String>,
    /// Time spent waiting for the user's approval (not shown as run time).
    pub waited: Duration,
    /// Waiting for an approval since then.
    pub wait_from: Option<Instant>,
}

impl Tool {
    /// Run time so far or in total, without approval waits.
    pub fn run_time(&self) -> Duration {
        let total = self.duration.unwrap_or_else(|| self.started.elapsed());
        let waiting = self.wait_from.map(|w| w.elapsed()).unwrap_or_default();
        total.saturating_sub(self.waited + waiting)
    }
}

/// A script host call shown under its script row.
pub struct Inner {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub output: Option<String>,
    pub is_error: bool,
    pub started: Instant,
    pub duration: Option<Duration>,
    /// Waiting for the user's approval.
    pub waiting: bool,
}

/// A tool call the model is still writing (from argument deltas).
pub struct Pending {
    pub index: usize,
    pub id: String,
    pub name: String,
    pub args: String,
    /// The key field parsed from the partial JSON (path, command, …).
    pub key: String,
    /// write/edit: the content written so far.
    pub preview: Option<String>,
    /// Set when the stream reset or the turn ended without a `ToolStart`.
    pub abandoned: Option<Instant>,
}

/// The color family of a [`Card`] row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Info,
    Warn,
    Ok,
    Err,
    /// Guidance and other advisory notes.
    Accent,
}

/// One part of a card's expanded body.
#[derive(Clone, Debug)]
pub enum CardPart {
    Md(String),
    /// A proposed write/edit: path, old text, new text, whole-file write.
    Diff {
        path: String,
        old: String,
        new: String,
        write: bool,
    },
}

/// A compact, collapsible notice row with a one-line head and an optional
/// body (scope requests, subscription changes, guidance, prune). `key`
/// identifies a row that updates in place (a scope request id).
#[derive(Clone, Debug)]
pub struct Card {
    pub key: String,
    pub icon: String,
    pub tone: Tone,
    pub head: String,
    pub body: Vec<CardPart>,
}

pub enum Kind {
    User(String),
    /// A compact collapsible notice (engine-o rows, prune, local runs).
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
    /// Older context files still on disk (D103): the agent and their
    /// numbers, oldest first; loaded one at a time when scrolled to.
    Older {
        agent: String,
        contexts: Vec<u32>,
        loading: bool,
    },
    /// A question answered through its fork (D64): question id, summary.
    /// Its button reopens the fork's saved chat.
    ForkAnswer {
        question: String,
        summary: String,
    },
}

/// Inner scroll box geometry from the last render.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoxInfo {
    pub total: usize,
    pub visible: usize,
    pub offset: usize,
    pub max: usize,
    /// The box follows its tail while not scrolled.
    pub follow: bool,
}

#[derive(Default)]
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub meta: Vec<Meta>,
    pub boxinfo: Option<BoxInfo>,
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

pub struct Entry {
    pub kind: Kind,
    pub expanded: bool,
    /// Assistant text: the segment is finished (code/table copy buttons appear).
    pub done: bool,
    /// Assistant text: the last segment of a finished turn. It carries the
    /// turn's copy row, which copies every segment of the turn; earlier
    /// segments show their own copy row only while hovered.
    pub turn_last: bool,
    /// User message: selected as an anchor (action row shown).
    pub anchored: bool,
    /// Inner box offset; None follows the tail (live) or sits at the top.
    pub box_scroll: Option<usize>,
    /// When a user or agent message was written (live: now; replay: the
    /// journal's `ts`), shown right-aligned on its first row.
    pub ts: Option<chrono::DateTime<chrono::Local>>,
    /// D117: whether `ts` is shown (user messages always; agent text on a
    /// turn's first segment, then only after a minute since the last shown).
    pub show_ts: bool,
    rev: u64,
    cache: Option<(Key, Rendered)>,
    /// The row count of a dropped cache (D103): far off-screen entries keep
    /// only their height until they come back into view.
    height: Option<(Key, usize)>,
}

impl Entry {
    pub fn new(kind: Kind, expanded: bool) -> Self {
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
    /// Drop the wrapped and highlighted rows, keeping the height (D103).
    pub fn trim(&mut self) {
        if let Some((k, r)) = self.cache.take() {
            self.height = Some((k, r.lines.len()));
        }
        if let Kind::Earlier(children) = &mut self.kind {
            children.iter_mut().for_each(Entry::trim);
        }
    }
    pub fn cached(&self) -> bool {
        self.cache.is_some()
    }
    pub fn touch(&mut self) {
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
            let (rows, boxinfo) = render_stamped(self, width, tick);
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
    /// Scroll the inner box by `delta` rows; false when it cannot move that
    /// way (the outer transcript should scroll instead).
    pub fn scroll_box(&mut self, delta: isize) -> bool {
        let Some(info) = self.cache.as_ref().and_then(|(_, r)| r.boxinfo) else {
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
    pub fn boxinfo(&self) -> Option<BoxInfo> {
        self.cache.as_ref().and_then(|(_, r)| r.boxinfo)
    }
}

fn has_body(t: &Tool) -> bool {
    matches!(t.name.as_str(), "write" | "edit" | "todo")
}

#[derive(Default)]
pub struct Transcript {
    pub entries: Vec<Entry>,
    /// Index of the first entry of the current model attempt; a stream reset
    /// discards assistant text and reasoning from there on.
    attempt_start: usize,
    pub show_reasoning: bool,
    /// Bumped whenever entries are removed or reordered (selections and
    /// anchors by index become invalid).
    pub generation: u64,
}

impl Transcript {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.attempt_start = 0;
        self.generation += 1;
    }
    pub fn push(&mut self, kind: Kind) {
        self.seal_last();
        let expanded = matches!(kind, Kind::Reasoning(_)) && self.show_reasoning;
        self.entries.push(Entry::new(kind, expanded));
        self.decide_stamp(self.entries.len() - 1);
    }
    /// D117 for entry `i`: agent text shows its time on the first segment
    /// of a turn, then only when a minute has passed since the last time
    /// shown. A turn starts at a user message, an agent or tree message, a
    /// question answer or a compaction. It depends only on the entries, so
    /// a replay stamps exactly as the live view did.
    fn decide_stamp(&mut self, i: usize) {
        let Some(e) = self.entries.get(i) else { return };
        let show = match e.kind {
            Kind::User(_) => true,
            Kind::Assistant(_) => {
                let Some(ts) = e.ts else { return };
                let mut show = true;
                for prev in self.entries[..i].iter().rev() {
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
        if self.entries[i].show_ts != show {
            self.entries[i].show_ts = show;
        }
    }
    /// Re-decide every entry's time (after a replay filled the times in).
    pub fn restamp(&mut self) {
        for i in 0..self.entries.len() {
            self.decide_stamp(i);
        }
    }
    /// The previous assistant segment is finished once anything follows it.
    fn seal_last(&mut self) {
        if let Some(e) = self.entries.last_mut()
            && matches!(e.kind, Kind::Assistant(_))
            && !e.done
        {
            e.done = true;
        }
    }
    /// Mark every assistant segment finished (turn end, replay) and the
    /// last segment of each turn as carrying the turn's copy row.
    pub fn seal(&mut self) {
        // Settled retry rows read as history: "retried 3× · reason".
        for e in &mut self.entries {
            if let Kind::Notice(Level::Warn, t) = &mut e.kind
                && let Some(rest) = t.strip_prefix("reconnecting · attempt ")
            {
                let n = rest.split(' ').next().unwrap_or("1").to_string();
                *t = format!(
                    "network dropped · {n} reconnect attempt{}",
                    if n == "1" { "" } else { "s" }
                );
                e.touch();
            }
            if let Kind::Notice(Level::Warn, t) = &mut e.kind
                && let Some(rest) = t.strip_prefix("retry ")
                && let Some((n, after)) = rest.split_once(" in ")
                && n.chars().all(|c| c.is_ascii_digit())
            {
                let reason = after.split_once(" · ").map(|(_, r)| r);
                *t = match reason {
                    Some(r) => format!("retried {n}× · {r}"),
                    None => format!("retried {n}×"),
                };
                e.touch();
            }
        }
        let mut last: Option<usize> = None;
        for i in 0..self.entries.len() {
            match self.entries[i].kind {
                Kind::Assistant(_) => {
                    self.entries[i].done = true;
                    self.entries[i].turn_last = false;
                    last = Some(i);
                }
                Kind::User(_) => {
                    if let Some(l) = last.take() {
                        self.entries[l].turn_last = true;
                    }
                }
                _ => {}
            }
        }
        if let Some(l) = last {
            self.entries[l].turn_last = true;
        }
    }
    /// The markdown of every assistant segment in the turn that entry `i`
    /// ends, joined by blank lines.
    pub fn turn_markdown(&self, i: usize) -> String {
        let start = self.entries[..i]
            .iter()
            .rposition(|e| matches!(e.kind, Kind::User(_)))
            .map_or(0, |u| u + 1);
        self.entries[start..=i]
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::Assistant(m) => Some(m.trim()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    /// A loaded older context (D103) goes in just below the "on disk" row,
    /// above the contexts loaded before it.
    pub fn insert_earlier(&mut self, n: u32, entries: Vec<Entry>) {
        let Some(Kind::Older {
            contexts, loading, ..
        }) = self.entries.first_mut().map(|e| &mut e.kind)
        else {
            return;
        };
        contexts.retain(|c| *c != n);
        *loading = false;
        let empty = contexts.is_empty();
        self.entries[0].touch();
        let mut added = 0isize;
        if !entries.is_empty() {
            self.entries
                .insert(1, Entry::new(Kind::Earlier(entries), false));
            added += 1;
        }
        if empty {
            self.entries.remove(0);
            added -= 1;
        }
        self.attempt_start = (self.attempt_start as isize + added).max(0) as usize;
        self.generation += 1;
    }
    /// The next older context to load, marking the row as loading.
    pub fn take_older(&mut self) -> Option<(String, u32)> {
        match self.entries.first_mut().map(|e| (&mut e.kind, &mut e.rev)) {
            Some((
                Kind::Older {
                    agent,
                    contexts,
                    loading,
                },
                rev,
            )) if !*loading => {
                let n = *contexts.last()?;
                *loading = true;
                *rev += 1;
                Some((agent.clone(), n))
            }
            _ => None,
        }
    }
    /// Fold everything so far into one collapsed "earlier context" section.
    pub fn fold_earlier(&mut self) {
        self.seal();
        let entries: Vec<Entry> = std::mem::take(&mut self.entries)
            .into_iter()
            .filter(|e| !matches!(e.kind, Kind::Compacting(_) | Kind::Pending(_)))
            .collect();
        if !entries.is_empty() {
            self.entries.push(Entry::new(Kind::Earlier(entries), false));
        }
        self.attempt_start = self.entries.len();
        self.generation += 1;
    }
    pub fn compacting(&mut self) {
        self.push(Kind::Compacting(String::new()));
    }
    pub fn append_compacting(&mut self, delta: &str) -> bool {
        if let Some(e) = self.entries.last_mut()
            && let Kind::Compacting(text) = &mut e.kind
        {
            text.push_str(delta);
            e.touch();
            return true;
        }
        false
    }
    pub fn end_compacting(&mut self) {
        let before = self.entries.len();
        self.entries
            .retain(|e| !matches!(e.kind, Kind::Compacting(_)));
        if self.entries.len() != before {
            self.generation += 1;
        }
    }
    pub fn begin_attempt(&mut self) {
        self.attempt_start = self.entries.len();
    }
    /// Give the last entry its journal time (replay).
    pub fn stamp_last(&mut self, rfc3339: &str) {
        if let Some(e) = self.entries.last_mut()
            && let Ok(t) = chrono::DateTime::parse_from_rfc3339(rfc3339)
        {
            e.ts = Some(t.with_timezone(&chrono::Local));
            e.touch();
            self.decide_stamp(self.entries.len() - 1);
        }
    }
    pub fn notice(&mut self, level: Level, text: impl Into<String>) {
        self.push(Kind::Notice(level, text.into()));
    }
    /// Add a card, or update the one with the same non-empty key in place
    /// (its expanded state is kept).
    pub fn card(&mut self, card: Card) {
        if !card.key.is_empty()
            && let Some(e) = self
                .entries
                .iter_mut()
                .rev()
                .find(|e| matches!(&e.kind, Kind::Card(c) if c.key == card.key))
        {
            e.kind = Kind::Card(Box::new(card));
            e.touch();
            return;
        }
        self.push(Kind::Card(Box::new(card)));
    }
    pub fn append_text(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(e) = self.entries.last_mut()
            && let Kind::Assistant(text) = &mut e.kind
        {
            text.push_str(delta);
            e.touch();
            return;
        }
        // A new segment (after tools/reasoning) starts its own entry, which
        // renders separated from the previous text.
        let text = delta.trim_start();
        if !text.is_empty() {
            self.push(Kind::Assistant(text.to_string()));
        }
    }
    pub fn append_reasoning(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(e) = self.entries.last_mut()
            && let Kind::Reasoning(text) = &mut e.kind
        {
            text.push_str(delta);
            e.touch();
            return;
        }
        let text = delta.trim_start();
        if !text.is_empty() {
            self.push(Kind::Reasoning(text.to_string()));
        }
    }
    pub fn stream_reset(&mut self) {
        let start = self.attempt_start.min(self.entries.len());
        let mut i = start;
        let before = self.entries.len();
        while i < self.entries.len() {
            if matches!(
                self.entries[i].kind,
                Kind::Assistant(_) | Kind::Reasoning(_)
            ) {
                self.entries.remove(i);
            } else {
                i += 1;
            }
        }
        if self.entries.len() != before {
            self.generation += 1;
        }
        self.abandon_pending();
    }
    /// A tool-call argument fragment: create or grow the pending row.
    pub fn tool_delta(
        &mut self,
        index: usize,
        id: Option<String>,
        name: Option<String>,
        args: &str,
    ) {
        let found = self
            .entries
            .iter_mut()
            .rev()
            .find_map(|e| match &mut e.kind {
                Kind::Pending(p)
                    if p.index == index
                        && p.abandoned.is_none()
                        && id.as_deref().is_none_or(|i| p.id.is_empty() || p.id == i) =>
                {
                    Some(p)
                }
                _ => None,
            });
        let p = match found {
            Some(p) => p,
            None => {
                self.push(Kind::Pending(Box::new(Pending {
                    index,
                    id: String::new(),
                    name: String::new(),
                    args: String::new(),
                    key: String::new(),
                    preview: None,
                    abandoned: None,
                })));
                match &mut self.entries.last_mut().unwrap().kind {
                    Kind::Pending(p) => p,
                    _ => unreachable!(),
                }
            }
        };
        if let Some(id) = id.filter(|i| !i.is_empty()) {
            p.id = id;
        }
        if let Some(name) = name {
            p.name.push_str(&name);
        }
        p.args.push_str(args);
        p.key = pending_key(&p.name, &p.args);
        p.preview = match p.name.as_str() {
            "write" => partial_field(&p.args, &["content"]),
            "edit" => partial_field(&p.args, &["new_string"]),
            _ => None,
        };
    }
    /// Abandon every pending row (stream reset, retry, turn end).
    /// An interrupt stopped the stream (D74): the text so far stays as a
    /// finished segment marked interrupted; tentative calls are dropped.
    pub fn interrupted(&mut self) {
        self.abandon_pending();
        if let Some(e) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| matches!(e.kind, Kind::Assistant(_)))
        {
            e.done = true;
            e.touch();
        }
        self.notice(Level::Warn, "interrupted · the partial reply above is kept");
        self.attempt_start = self.entries.len();
    }
    pub fn abandon_pending(&mut self) {
        for e in &mut self.entries {
            if let Kind::Pending(p) = &mut e.kind
                && p.abandoned.is_none()
            {
                p.abandoned = Some(Instant::now());
                e.touch();
            }
        }
    }
    /// Drop abandoned pending rows after a moment; true when any went.
    pub fn prune_abandoned(&mut self) -> bool {
        let before = self.entries.len();
        self.entries.retain(
            |e| !matches!(&e.kind, Kind::Pending(p) if p.abandoned.is_some_and(|t| t.elapsed() >= ABANDONED_FOR)),
        );
        let changed = self.entries.len() != before;
        if changed {
            self.generation += 1;
        }
        changed
    }
    pub fn has_abandoned(&self) -> bool {
        self.entries
            .iter()
            .any(|e| matches!(&e.kind, Kind::Pending(p) if p.abandoned.is_some()))
    }
    /// A retry notice: replaces the previous one when nothing came between,
    /// so a failing call shows one row counting its attempts.
    pub fn retry_notice(&mut self, text: &str) {
        if let Some(e) = self.entries.last_mut()
            && let Kind::Notice(Level::Warn, t) = &mut e.kind
            && (t.starts_with("retry ") || t.starts_with("reconnecting · "))
        {
            *t = text.to_string();
            e.touch();
            return;
        }
        self.notice(Level::Warn, text.to_string());
    }
    pub fn tool_start(&mut self, id: String, name: String, args: Value) {
        let tool = Tool {
            id: id.clone(),
            name: name.clone(),
            args,
            output: None,
            is_error: false,
            started: Instant::now(),
            duration: None,
            replayed: false,
            child: None,
            review: None,
            inner: vec![],
            edits: vec![],
            moved: None,
            waited: Duration::ZERO,
            wait_from: None,
        };
        // The pending row turns into the real row in place.
        let live = |p: &Pending| p.abandoned.is_none();
        let at = self
            .entries
            .iter()
            .position(|e| matches!(&e.kind, Kind::Pending(p) if live(p) && !p.id.is_empty() && p.id == id))
            .or_else(|| {
                self.entries.iter().position(
                    |e| matches!(&e.kind, Kind::Pending(p) if live(p) && (p.id.is_empty() || p.name == name)),
                )
            });
        match at {
            Some(i) => {
                let e = &mut self.entries[i];
                e.kind = Kind::Tool(Box::new(tool));
                e.box_scroll = None;
                e.touch();
            }
            None => self.push(Kind::Tool(Box::new(tool))),
        }
    }
    pub fn replayed_tool(&mut self, id: String, name: String, args: Value) {
        self.push(Kind::Tool(Box::new(Tool {
            id,
            name,
            args,
            output: None,
            is_error: false,
            started: Instant::now(),
            duration: Some(Duration::ZERO),
            replayed: true,
            child: None,
            review: None,
            inner: vec![],
            edits: vec![],
            moved: None,
            waited: Duration::ZERO,
            wait_from: None,
        })));
    }
    /// The engine supplies the exact call awaiting approval and, for a
    /// script host call, its containing script: that row waits, and so
    /// does the inner call under it.
    pub fn approval_wait(&mut self, call_id: &str, parent: Option<&str>) {
        let row = parent.unwrap_or(call_id);
        if let Some(e) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| matches!(&e.kind, Kind::Tool(t) if t.id == row))
            && let Kind::Tool(t) = &mut e.kind
        {
            t.wait_from.get_or_insert_with(Instant::now);
            if parent.is_some()
                && let Some(c) = t.inner.iter_mut().rev().find(|c| c.id == call_id)
            {
                c.waiting = true;
            }
            e.touch();
        }
    }
    /// Approvals answered: waiting rows resume their run time.
    pub fn approval_done(&mut self) {
        for e in self.entries.iter_mut().rev() {
            if let Kind::Tool(t) = &mut e.kind
                && let Some(w) = t.wait_from.take()
            {
                t.waited += w.elapsed();
                t.inner.iter_mut().for_each(|c| c.waiting = false);
                e.touch();
            }
        }
    }
    pub fn tool_end(
        &mut self,
        id: &str,
        output: String,
        is_error: bool,
        details: ToolResultDetails,
    ) {
        for e in self.entries.iter_mut().rev() {
            if let Kind::Tool(t) = &mut e.kind
                && t.id == id
            {
                t.output = Some(output);
                t.is_error = is_error;
                t.edits = details.edits;
                // The engine's run time (it includes approval waits, which
                // the live row subtracts), so a replay shows what live did.
                match details.duration_ms {
                    Some(ms) => {
                        t.duration = Some(Duration::from_millis(ms));
                        t.replayed = false;
                    }
                    None => t.duration = Some(t.started.elapsed()),
                }
                if let Some(w) = t.wait_from.take() {
                    t.waited += w.elapsed();
                }
                e.touch();
                break;
            }
        }
        self.attempt_start = self.entries.len();
    }
    /// A finished call from a structured record (fork chat, replay): a
    /// row with its arguments, result and recorded run time.
    pub fn recorded_tool(&mut self, call: ToolCall, result: Option<ToolResultData>) {
        self.replayed_tool(call.id.clone(), call.name, call.arguments);
        if let Some(r) = result {
            self.tool_end(&call.id, r.output, r.is_error, r.details);
        }
    }
    /// A steer (or an interrupt) moved foreground call `id` to the
    /// background as job `job` (D72): from here on it is a background job
    /// row, as if it had been started with `background: true`.
    pub fn moved_to_background(&mut self, id: &str, job: &str, reason: &str) {
        for e in self.entries.iter_mut().rev() {
            if let Kind::Tool(t) = &mut e.kind
                && t.id == id
            {
                if let Some(a) = t.args.as_object_mut() {
                    a.insert("background".into(), Value::Bool(true));
                }
                let mut result = serde_json::json!({ "jobId": job, "status": "running" });
                if let Some(c) = &t.child {
                    result["agentId"] = c.id.clone().into();
                }
                t.output = Some(result.to_string());
                t.is_error = false;
                t.duration = Some(t.started.elapsed());
                t.moved = Some(reason.to_string());
                e.touch();
                break;
            }
        }
    }
    /// A sidecar review started (`result` None) or finished for call `id`.
    /// False when the call has no row (yet).
    pub fn review(
        &mut self,
        id: &str,
        model: String,
        result: Option<(bool, String, String)>,
    ) -> bool {
        for e in self.entries.iter_mut().rev() {
            if let Kind::Tool(t) = &mut e.kind
                && t.id == id
            {
                t.review = Some(Review { model, result });
                e.touch();
                return true;
            }
        }
        false
    }
    /// A host call of a script, on its containing script row (`parent`),
    /// else the row that launched job `job`, else the newest running
    /// script row. Started calls are added; results update them by id.
    pub fn script_call(
        &mut self,
        job: Option<&str>,
        parent: Option<&str>,
        call: ToolCall,
        result: Option<ToolResultData>,
        status: &str,
    ) {
        let by_parent = parent.and_then(|p| {
            self.entries
                .iter()
                .rposition(|e| matches!(&e.kind, Kind::Tool(t) if t.id == p))
        });
        let at = by_parent
            .or_else(|| job.and_then(|j| {
                self.entries.iter().rposition(|e| {
                    matches!(&e.kind, Kind::Tool(t) if t.name == "script"
                        && t.output.as_deref().is_some_and(|o| o.contains(j)))
                })
            }))
            .or_else(|| {
                self.entries.iter().rposition(
                    |e| matches!(&e.kind, Kind::Tool(t) if t.name == "script" && t.output.is_none()),
                )
            });
        let Some(i) = at else { return };
        let e = &mut self.entries[i];
        let Kind::Tool(t) = &mut e.kind else { return };
        let pos = t.inner.iter().rposition(|c| c.id == call.id);
        let c = match pos {
            Some(p) => &mut t.inner[p],
            None => {
                t.inner.push(Inner {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    args: call.arguments.clone(),
                    output: None,
                    is_error: false,
                    started: Instant::now(),
                    duration: None,
                    waiting: false,
                });
                t.inner.last_mut().unwrap()
            }
        };
        if let Some(r) = result {
            c.duration = Some(
                r.details
                    .duration_ms
                    .map(Duration::from_millis)
                    .unwrap_or_else(|| c.started.elapsed()),
            );
            c.output = Some(r.output);
            c.is_error = r.is_error;
            c.waiting = false;
        } else if status == "error" || status == "completed" {
            c.duration = Some(c.started.elapsed());
            c.output.get_or_insert_with(String::new);
            c.is_error = status == "error";
            c.waiting = false;
        }
        e.touch();
    }
    pub fn todo(&mut self, items: Vec<TodoItem>) {
        // The todo tool row renders its own checklist; refresh it from the event.
        for e in self.entries.iter_mut().rev() {
            if let Kind::Tool(t) = &mut e.kind
                && t.name == "todo"
            {
                t.args = serde_json::json!({ "items": items.iter().map(|i| serde_json::json!({"content": i.content, "status": i.status})).collect::<Vec<_>>() });
                e.touch();
                break;
            }
        }
    }
    /// Mark tools that never finished (turn ended or was cancelled).
    pub fn abandon_running(&mut self) {
        self.abandon_pending();
        for e in &mut self.entries {
            if let Kind::Tool(t) = &mut e.kind
                && t.output.is_none()
            {
                t.output = Some(if t.replayed {
                    "no result recorded".into()
                } else {
                    "cancelled".into()
                });
                t.is_error = true;
                t.duration = Some(t.started.elapsed());
                for c in t.inner.iter_mut().filter(|c| c.output.is_none()) {
                    c.output = Some(String::new());
                    c.is_error = true;
                    c.waiting = false;
                }
                if let Some(r) = &mut t.review
                    && r.result.is_none()
                {
                    t.review = None;
                }
                e.touch();
            }
        }
    }
    pub fn set_show_reasoning(&mut self, show: bool) {
        self.show_reasoning = show;
        for e in &mut self.entries {
            if matches!(e.kind, Kind::Reasoning(_)) {
                e.expanded = show;
            }
        }
    }
    /// `task` rows whose link is missing or contradicted by the result:
    /// (entry index, child id hint). The hint is the `continue` argument or
    /// the result's `agentId` — ids, never prompt text.
    pub fn unlinked_tasks(&self) -> Vec<(usize, Option<String>)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match &e.kind {
                Kind::Tool(t) if t.name == "task" => {
                    let result = t
                        .output
                        .as_deref()
                        .and_then(|o| serde_json::from_str::<Value>(o).ok())
                        .and_then(|v| v["agentId"].as_str().map(str::to_string));
                    let hint = t.args["continue"].as_str().map(str::to_string).or(result);
                    match (&t.child, &hint) {
                        (None, _) => Some((i, hint)),
                        // A provisional link the result contradicts.
                        (Some(c), Some(h)) if c.id != *h => Some((i, hint)),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect()
    }
    /// The `task` row a newly started child belongs to: the call `call_id`
    /// when the engine names it, else a `continue` of that agent, else the
    /// oldest running unlinked new-task row (calls execute in order).
    /// Never matched on prompt text: identical prompts are common.
    pub fn task_row_for(&self, agent: &str, call_id: Option<&str>) -> Option<usize> {
        fn running_task(e: &Entry) -> Option<&Tool> {
            match &e.kind {
                Kind::Tool(t) if t.name == "task" && t.output.is_none() => Some(t),
                _ => None,
            }
        }
        if let Some(call) = call_id {
            return self
                .entries
                .iter()
                .position(|e| matches!(&e.kind, Kind::Tool(t) if t.id == call));
        }
        self.entries
            .iter()
            .position(|e| {
                running_task(e).is_some_and(|t| {
                    t.child.is_none() && t.args["continue"].as_str() == Some(agent)
                })
            })
            .or_else(|| {
                self.entries.iter().position(|e| {
                    running_task(e)
                        .is_some_and(|t| t.child.is_none() && !t.args["continue"].is_string())
                })
            })
    }
    pub fn link_task(&mut self, i: usize, link: ChildLink) {
        if let Some(e) = self.entries.get_mut(i)
            && let Kind::Tool(t) = &mut e.kind
        {
            t.child = Some(link);
            e.touch();
        }
    }
    /// Refresh the status shown on `task` rows linked to `id`.
    pub fn child_status(&mut self, id: &str, status: &str) {
        for e in &mut self.entries {
            if let Kind::Tool(t) = &mut e.kind
                && let Some(c) = &mut t.child
                && c.id == id
                && c.status != status
            {
                c.status = status.to_string();
                e.touch();
            }
        }
    }
    /// Whether a blank separator row precedes entry `i`.
    pub fn gap_before(&self, i: usize) -> bool {
        let history = |e: &Entry| matches!(e.kind, Kind::Earlier(_) | Kind::Older { .. });
        i > 0
            && !(self.entries[i].is_row() && self.entries[i - 1].is_row())
            // Earlier contexts stack as one list of headers.
            && !(history(&self.entries[i]) && history(&self.entries[i - 1]))
    }
    /// Indices of user messages, oldest first.
    pub fn user_entries(&self) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| matches!(self.entries[i].kind, Kind::User(_)))
            .collect()
    }
    /// Anchor one user message (None clears).
    pub fn set_anchor(&mut self, i: Option<usize>) {
        for (j, e) in self.entries.iter_mut().enumerate() {
            let on = Some(j) == i && matches!(e.kind, Kind::User(_));
            if e.anchored != on {
                e.anchored = on;
            }
        }
    }
    pub fn anchor(&self) -> Option<usize> {
        self.entries.iter().position(|e| e.anchored)
    }
}

/// Lay rows into an inner scroll box of `visible` rows plus an indicator row.
fn boxed(
    out: &mut Vec<Row>,
    body: Vec<Row>,
    visible: usize,
    scroll: Option<usize>,
    follow: bool,
    gutter: &Span<'static>,
) -> Option<BoxInfo> {
    if body.len() <= visible {
        out.extend(body);
        return None;
    }
    let total = body.len();
    let max = total - visible;
    let offset = scroll.unwrap_or(if follow { max } else { 0 }).min(max);
    for (line, mut meta) in body.into_iter().skip(offset).take(visible) {
        meta.boxed = true;
        out.push((line, meta));
    }
    let t = theme();
    let below = total - offset - visible;
    let text = match (offset, below) {
        (0, b) => format!("↓ {b} more · wheel or ↑↓ scrolls the box"),
        (a, 0) => format!("↑ {a} more"),
        (a, b) => format!("↑ {a} more · ↓ {b} more"),
    };
    out.push((
        Line::from(vec![gutter.clone(), Span::styled(text, t.faint())]),
        Meta {
            skip: true,
            boxed: true,
            ..Meta::default()
        },
    ));
    Some(BoxInfo {
        total,
        visible,
        offset,
        max,
        follow,
    })
}

fn render(entry: &Entry, width: usize, tick: usize) -> (Vec<Row>, Option<BoxInfo>) {
    let t = theme();
    let width = width.max(8);
    let mut boxinfo = None;
    let rows = match &entry.kind {
        Kind::User(text) => {
            let bar = Span::styled("▌ ", Style::default().fg(t.accent));
            // Paste blocks show as one chip row each (D53); a click opens
            // them. Attachment chips (D79) stay inline as labels and are
            // repeated as clickable chips in a tray under the text.
            let mut body: Vec<Row> = vec![];
            let mut n = 0usize;
            let mut cur: Vec<Span<'static>> = vec![];
            let mut tray: Vec<(String, bool)> = vec![];
            let segs = crate::paste::parse(text);
            let last = segs.len().saturating_sub(1);
            let line =
                |cur: &mut Vec<Span<'static>>| (Line::from(std::mem::take(cur)), Meta::gutter(0));
            for (si, seg) in segs.iter().enumerate() {
                match seg {
                    crate::paste::Seg::Text(part) => {
                        let mut part = *part;
                        let paste_at =
                            |i: usize| matches!(segs.get(i), Some(crate::paste::Seg::Paste { .. }));
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
            if entry.anchored {
                rows.push(button_row(
                    "  ↳ ",
                    &[Btn::Fork, Btn::CopyUser],
                    Style::default().fg(t.dim),
                ));
            }
            rows
        }
        Kind::Assistant(text) | Kind::Info(text) => {
            let assistant = matches!(entry.kind, Kind::Assistant(_));
            let buttons = assistant && entry.done;
            let turn_row = assistant && entry.turn_last;
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
            if !entry.expanded {
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
                    entry.box_scroll,
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
            let fold = all.len() > NOTICE_LINES && !entry.expanded;
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
        Kind::Card(card) => render_card(card, entry.expanded, width),
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
            let (rows, b) = render_tool(tool, entry, width, tick);
            boxinfo = b;
            rows
        }
        Kind::Pending(p) => {
            let (rows, b) = render_pending(p, entry.box_scroll, width, tick);
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
            let marker = if entry.expanded { "▾" } else { "▸" };
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
            if entry.expanded {
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
            let marker = if entry.expanded { "▾" } else { "▸" };
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
            if entry.expanded {
                let gutter = Span::styled("  ┃ ", Style::default().fg(t.rule));
                for (i, e) in entries.iter().enumerate() {
                    if i > 0 && !(e.is_row() && entries[i - 1].is_row()) {
                        out.push((Line::from(gutter.clone()), Meta::gutter(4)));
                    }
                    for (l, m) in cached_rows(e, width.saturating_sub(4), tick) {
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
fn cached_rows(e: &Entry, width: usize, tick: usize) -> Vec<Row> {
    match &e.cache {
        Some((k, r)) if *k == e.key(width) => r
            .lines
            .iter()
            .cloned()
            .zip(r.meta.iter().cloned())
            .collect(),
        _ => render_stamped(e, width, tick).0,
    }
}

/// A message time: "14:05" today, "Oct 27 14:05" on another day.
pub fn fmt_stamp(t: &chrono::DateTime<chrono::Local>) -> String {
    if t.date_naive() == chrono::Local::now().date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%b %-d %H:%M").to_string()
    }
}

/// [`render`] plus the message time right-aligned on the first row. The
/// first row keeps its full width when the time fits beside it; otherwise
/// the message wraps narrower so the time never covers text.
fn render_stamped(entry: &Entry, width: usize, tick: usize) -> (Vec<Row>, Option<BoxInfo>) {
    let Some(ts) = entry.ts.filter(|_| entry.show_ts && width >= 40) else {
        return render(entry, width, tick);
    };
    let label = fmt_stamp(&ts);
    // Two columns of air before the time, one after it.
    let need = label.width() + 3;
    let first_w = |rows: &[Row]| -> usize {
        rows.first()
            .map(|(l, _)| l.spans.iter().map(|s| s.content.width()).sum())
            .unwrap_or(0)
    };
    let (mut rows, boxinfo) = render(entry, width, tick);
    if rows.is_empty() {
        return (rows, boxinfo);
    }
    if first_w(&rows) + need > width {
        let (narrow, b) = render(entry, width - need, tick);
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

static CWD: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Paths under this directory are shown relative to it in tool rows.
pub fn set_cwd(cwd: &std::path::Path) {
    let _ = CWD.set(format!("{}/", cwd.display()));
}

/// A write/edit result that is a D120 scope request: (status, owner). The
/// no-wait form is `{requestId, owner, status, applied:false}`; a waited
/// one is the whole request (`requester`, `owner`, `status`).
fn scope_result(output: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(output.lines().next()?.trim()).ok()?;
    let is_request =
        v.get("requestId").is_some() || (v.get("requester").is_some() && v.get("call").is_some());
    if !is_request {
        return None;
    }
    let status = v["status"].as_str()?.to_string();
    let owner = v["owner"].as_str().unwrap_or("its owner").to_string();
    Some((status, owner))
}

/// A path relative to the session directory when it is inside it.
pub fn rel_path(p: &std::path::Path) -> String {
    rel(&p.display().to_string())
}

fn rel(v: &str) -> String {
    CWD.get()
        .and_then(|c| v.strip_prefix(c.as_str()))
        .unwrap_or(v)
        .to_string()
}

/// The argument that identifies a call at a glance.
fn key_arg(tool: &Tool) -> String {
    call_arg(&tool.name, &tool.args)
}

/// [`key_arg`] for any call by name and arguments (script inner calls too).
fn call_arg(name: &str, a: &Value) -> String {
    if matches!(name, "mcp.search" | "mcp.describe") {
        return generic_arg(a);
    }
    // Script host functions such as `criteria.propose`: the tool's own
    // summary, without repeating the action the name already shows.
    if let Some((base, action)) = name.split_once('.')
        && a.get("action").is_none()
        && let Some(obj) = a.as_object()
    {
        let mut with = obj.clone();
        with.insert("action".into(), Value::String(action.into()));
        let full = call_arg(base, &Value::Object(with));
        return full
            .strip_prefix(action)
            .map(|r| r.trim_start().to_string())
            .unwrap_or(full);
    }
    let s = |k: &str| rel(a[k].as_str().unwrap_or_default());
    match name {
        "read" => {
            let mut p = s("path");
            if let Some(o) = a["offset"].as_u64().filter(|&o| o > 1) {
                p.push_str(&format!(":{o}"));
            }
            p
        }
        "write" | "edit" => s("path"),
        "shell" => s("command").lines().next().unwrap_or_default().to_string(),
        "bash" => {
            let cmd = s("command").lines().next().unwrap_or_default().to_string();
            if a["background"].as_bool() == Some(true) {
                format!("{cmd}  (background)")
            } else {
                cmd
            }
        }
        "grep" => {
            let path = s("path");
            if path.is_empty() {
                s("pattern")
            } else {
                format!("{}  in {path}", s("pattern"))
            }
        }
        "glob" => s("pattern"),
        "script" => {
            let code = a["code"].as_str().unwrap_or_default();
            let first = code
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or_default();
            if a["background"].as_bool() == Some(true) {
                format!("{}  (background)", first.trim())
            } else {
                first.trim().to_string()
            }
        }
        // D131: the child's durable title, else the brief's first line.
        "task" => {
            let title = s("title");
            if title.trim().is_empty() {
                s("prompt").lines().next().unwrap_or_default().to_string()
            } else {
                title
            }
        }
        // `criteria {action, id?, text?, evidence?, kind?}` (D110).
        "criteria" => {
            let action = a["action"].as_str().unwrap_or("show");
            let id = a["id"]
                .as_str()
                .map(|i| format!("{} ", &i[..i.len().min(8)]))
                .unwrap_or_default();
            let kind = a["kind"]
                .as_str()
                .map(|k| format!("{k} "))
                .unwrap_or_default();
            let detail = [
                "text",
                "evidence",
                "change",
                "reason",
                "objective",
                "status",
            ]
            .iter()
            .find_map(|k| a[*k].as_str())
            .unwrap_or_default();
            format!(
                "{action} {kind}{id}{}",
                detail.lines().next().unwrap_or_default()
            )
            .trim_end()
            .to_string()
        }
        "todo" => {
            let items = todo_items(&a["items"]);
            let done = items.iter().filter(|i| i.status == "completed").count();
            format!("{done}/{} done", items.len())
        }
        // `question {questions: [{text, ...}]}`: the first question.
        "question" => {
            let qs = a["questions"].as_array().cloned().unwrap_or_default();
            let first = qs
                .first()
                .and_then(|q| q["text"].as_str())
                .or_else(|| a["text"].as_str())
                .unwrap_or_default();
            let first = first.lines().next().unwrap_or_default();
            if qs.len() > 1 {
                format!("{first}  +{} more", qs.len() - 1)
            } else {
                first.to_string()
            }
        }
        // `escalate {questionId, note?}`: the note says why.
        "escalate" => a["note"]
            .as_str()
            .map(|n| n.lines().next().unwrap_or_default().to_string())
            .unwrap_or_else(|| "question to the user".into()),
        "mcp" => format!(
            "{}.{}",
            a["server"].as_str().unwrap_or("?"),
            a["tool"].as_str().unwrap_or("?")
        ),
        _ => generic_arg(a),
    }
}

/// Any other call at a glance: its action and main string argument, else
/// its scalar arguments as `key=value`, else the compact JSON.
fn generic_arg(a: &Value) -> String {
    const KEYS: [&str; 12] = [
        "path",
        "file_path",
        "url",
        "query",
        "pattern",
        "prompt",
        "text",
        "message",
        "name",
        "symbol",
        "agent",
        "id",
    ];
    let action = a["action"].as_str().filter(|s| !s.is_empty());
    let main = KEYS
        .iter()
        .find_map(|k| a[*k].as_str().filter(|s| !s.trim().is_empty()))
        .map(|v| rel(v.lines().next().unwrap_or_default()));
    match (action, main) {
        (Some(ac), Some(m)) => format!("{ac} {m}"),
        (Some(ac), None) => ac.to_string(),
        (None, Some(m)) => m,
        (None, None) => {
            let Some(obj) = a.as_object() else {
                return String::new();
            };
            if obj.values().all(|v| !v.is_object() && !v.is_array()) {
                obj.iter()
                    .map(|(k, v)| match v.as_str() {
                        Some(s) => format!("{k}={s}"),
                        None => format!("{k}={v}"),
                    })
                    .collect::<Vec<_>>()
                    .join("  ")
            } else {
                a.to_string()
            }
        }
    }
}

/// A write/edit call as a unified diff with path headers (copy diff).
pub fn unified_diff(tool: &Tool) -> Option<String> {
    let path = rel(tool.args["path"].as_str().unwrap_or_default());
    if matches!(tool.name.as_str(), "edit" | "write") && !tool.edits.is_empty() {
        let new_file = tool
            .edits
            .iter()
            .all(|e| e.hunks.iter().all(|h| h.old_text.is_empty()));
        return Some(diff::unified_from_edits(&path, &tool.edits, new_file));
    }
    let (old, new, from) = match tool.name.as_str() {
        "edit" => (
            tool.args["old_string"].as_str().unwrap_or_default(),
            tool.args["new_string"].as_str().unwrap_or_default(),
            format!("a/{path}"),
        ),
        "write" => (
            "",
            tool.args["content"].as_str().unwrap_or_default(),
            "/dev/null".to_string(),
        ),
        _ => return None,
    };
    // Edit fragments rarely end in a newline; avoid "\ No newline" noise.
    let nl = |s: &str| {
        if s.is_empty() || s.ends_with('\n') {
            s.to_string()
        } else {
            format!("{s}\n")
        }
    };
    let (old, new) = (nl(old), nl(new));
    let diff = similar::TextDiff::from_lines(&old, &new);
    Some(
        diff.unified_diff()
            .context_radius(3)
            .header(&from, &format!("b/{path}"))
            .to_string(),
    )
}

/// Variation notes at the head of a tool result (self-evaluated writes,
/// auto-mode `improve`; D60, D104, D105): each as (summary, judge
/// rationale), plus the rest of the output. Candidate numbers are
/// zero-based and 0 is the original call:
/// "self-evaluated 3 candidates; applied variation 2: …" → "self-evaluated 3 · applied #2",
/// "… variation 0: …" → "self-evaluated 3 · kept original",
/// "synthesized from candidates [1,2]: …" → "synthesized from #1+#2".
pub fn variation_notes(output: &str) -> (Vec<(String, String)>, &str) {
    let mut notes = vec![];
    let mut rest = output;
    loop {
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        if let Some(after) = line.strip_prefix("synthesized from candidates ") {
            let (list, why) = after.split_once(": ").unwrap_or((after, ""));
            let nums: Vec<String> = list
                .trim_matches(['[', ']'])
                .split(',')
                .map(|n| n.trim())
                .filter(|n| !n.is_empty())
                .map(|n| format!("#{n}"))
                .collect();
            notes.push((
                format!("synthesized from {}", nums.join("+")),
                why.trim().to_string(),
            ));
            rest = tail;
            continue;
        }
        let Some((kind, after)) = ["self-evaluated ", "improved "]
            .iter()
            .find_map(|k| line.strip_prefix(k).map(|a| (k.trim(), a)))
        else {
            break;
        };
        let Some((count, after)) = after.split_once(" candidates; ") else {
            break;
        };
        let (how, why) = after.split_once(": ").unwrap_or((after, ""));
        let (why, invalid) = match why.strip_suffix("; synthesis invalid; used best single pick") {
            Some(w) => (w, true),
            None => (why, false),
        };
        let mut how = match how.strip_prefix("applied variation ") {
            Some("0") => "kept original".to_string(),
            Some(k) => format!("applied #{k}"),
            None => how.to_string(),
        };
        if invalid {
            how.push_str(" (synthesis invalid)");
        }
        notes.push((format!("{kind} {count} · {how}"), why.trim().to_string()));
        rest = tail;
    }
    (notes, rest)
}

const TAMPER: &str = "REVIEWER-GUIDANCE TAMPER";

/// A reviewer-guidance tamper flag in a tool result: (a short reading of the flag,
/// the output without it). JSON results carry it as `reviewerGuidanceTamper`.
fn tamper_flag(out: &str) -> (Option<String>, Option<String>) {
    if !out.contains(TAMPER) {
        return (None, None);
    }
    let (flag, rest) = match serde_json::from_str::<Value>(out.trim()) {
        Ok(Value::Object(mut o)) if o.contains_key("reviewerGuidanceTamper") => {
            let f = o
                .remove("reviewerGuidanceTamper")
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            (f, Value::Object(o).to_string())
        }
        _ => {
            let at = out.find(TAMPER).unwrap_or(out.len());
            (
                out[at..].trim().to_string(),
                out[..at].trim_end().to_string(),
            )
        }
    };
    let rest = rest.trim_end().trim_end_matches('[').trim_end().to_string();
    let detail = flag
        .trim_start_matches('[')
        .trim_end_matches(']')
        .strip_prefix(TAMPER)
        .map(|d| d.trim_start_matches(':').trim())
        .unwrap_or(&flag);
    // The usual flag reads short: what changed, what the engine did.
    let short = detail
        .split_once(" changed without a user-approved write; ")
        .map(|(path, after)| {
            let what = after.split(". ").next().unwrap_or(after);
            format!(
                "{} changed without your approval · {what} · approval asked",
                rel(path)
            )
        });
    (
        Some(format!(
            "reviewer guidance: {}",
            short.unwrap_or_else(|| rel(detail))
        )),
        Some(rest),
    )
}

/// A bash result (`[Error: ]exit: N\nstdout:\n…\nstderr:\n…`) for
/// display: stdout, then stderr, then the exit status when it is not 0.
/// The model still gets the labelled original.
fn bash_display(out: &str) -> Option<String> {
    let rest = out.strip_prefix("Error: ").unwrap_or(out);
    let (status, rest) = rest.strip_prefix("exit: ")?.split_once('\n')?;
    let rest = rest.strip_prefix("stdout:\n")?;
    let (stdout, stderr) = match rest.rfind("\nstderr:\n") {
        Some(i) => (&rest[..i], &rest[i + "\nstderr:\n".len()..]),
        None => (rest.strip_suffix("\nstderr:").unwrap_or(rest), ""),
    };
    let mut parts: Vec<String> = vec![];
    for p in [stdout, stderr] {
        let p = p.trim_end_matches('\n');
        if !p.trim().is_empty() {
            parts.push(p.to_string());
        }
    }
    if status != "0" {
        parts.push(format!("exit {status}"));
    }
    Some(parts.join("\n"))
}

/// A script result (`{"stdout": …, "value": …}`) for display: the printed
/// text, then `→ value` when the final expression has one.
fn script_display(out: &str) -> Option<String> {
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    let obj = v.as_object()?;
    // Guidance notices printed by the script show as their own row.
    let stdout = crate::engine::strip_guidance(obj.get("stdout")?.as_str()?.to_string());
    let mut parts: Vec<String> = vec![];
    if !stdout.trim().is_empty() {
        parts.push(stdout.trim_end_matches('\n').to_string());
    }
    match obj.get("value") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => {
            let s = crate::engine::strip_guidance(s.clone());
            parts.push(format!("→ {}", bash_display(&s).unwrap_or(s)))
        }
        Some(v) => parts.push(format!(
            "→ {}",
            serde_json::to_string_pretty(v).unwrap_or_default()
        )),
    }
    Some(parts.join("\n"))
}

fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60)
    }
}

fn fmt_chars(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}k chars", n as f64 / 1000.0)
    } else {
        format!("{n} chars")
    }
}

fn render_tool(
    tool: &Tool,
    entry: &Entry,
    width: usize,
    tick: usize,
) -> (Vec<Row>, Option<BoxInfo>) {
    let t = theme();
    let expanded = entry.expanded;
    let reviewing = tool.review.as_ref().is_some_and(|r| r.result.is_none());
    // D120: an out-of-scope write/edit returns a request, not a write.
    let scope = matches!(tool.name.as_str(), "write" | "edit")
        .then_some(tool.output.as_deref())
        .flatten()
        .and_then(scope_result);
    let (icon, color) = match (&tool.output, tool.is_error) {
        (None, _) if reviewing => (SPINNER[tick % SPINNER.len()], t.link),
        (None, _) => (SPINNER[tick % SPINNER.len()], t.yellow),
        _ if scope.as_ref().is_some_and(|s| s.0 != "applied") => {
            match scope.as_ref().unwrap().0.as_str() {
                "rejected" | "failed" => ("✗", t.red),
                _ => ("⇄", t.yellow),
            }
        }
        (Some(_), false) => ("✓", t.green),
        (Some(_), true) => ("✗", t.red),
    };
    let path = tool.args["path"].as_str().unwrap_or_default();
    let diff = match tool.name.as_str() {
        // A finished call: the engine's hunks, with line numbers.
        "edit" | "write" if !tool.edits.is_empty() => Some(diff::render_edits(path, &tool.edits)),
        "edit" => Some(diff::render_file(
            path,
            tool.args["old_string"].as_str().unwrap_or_default(),
            tool.args["new_string"].as_str().unwrap_or_default(),
            None,
        )),
        "write" => Some(diff::render_file(
            path,
            "",
            tool.args["content"].as_str().unwrap_or_default(),
            Some((1, 1)),
        )),
        _ => None,
    };
    let duration = tool.run_time();
    let mut tail = vec![];
    if let Some(r) = &tool.review {
        match &r.result {
            None => tail.push(Span::styled(
                format!("  reviewing with {}…", r.model),
                Style::default().fg(t.link),
            )),
            Some((true, _, _)) => tail.push(Span::styled("  ✓ safe", Style::default().fg(t.green))),
            Some((false, verdict, reason)) => {
                let why = reason.lines().next().unwrap_or_default();
                tail.push(Span::styled(
                    format!("  ✗ {verdict}: {}", truncate(why, 40)),
                    Style::default().fg(t.red),
                ))
            }
        }
    }
    // D115: a tamper flag the engine added to the result shows as its own
    // row; the output shows without it.
    let (tamper, stripped) = tool.output.as_deref().map(tamper_flag).unwrap_or_default();
    let (notes, body) = stripped
        .as_deref()
        .or(tool.output.as_deref())
        .map(variation_notes)
        .unwrap_or_default();
    if let Some(d) = &diff {
        tail.push(Span::styled(
            format!(" +{}", d.added),
            Style::default().fg(t.green),
        ));
        if d.removed > 0 {
            tail.push(Span::styled(
                format!(" -{}", d.removed),
                Style::default().fg(t.red),
            ));
        }
    }
    if let Some((status, owner)) = &scope
        && status != "applied"
    {
        let what = match status.as_str() {
            "pending" | "applying" => format!("requested from {owner} · not applied yet"),
            "granted" => "scope granted · not applied; retry".to_string(),
            "rejected" => format!("rejected by {owner} · not applied"),
            "stale" => "stale · re-read and retry".to_string(),
            "unavailable" => "owner unavailable · not applied".to_string(),
            other => format!("{other} · not applied"),
        };
        tail.push(Span::styled(
            format!("  {what}"),
            Style::default().fg(color),
        ));
    }
    // D101: a follow-up's context choice ("fresh" when the cache is cold).
    let task_context = (tool.name == "task")
        .then_some(tool.output.as_deref())
        .flatten()
        .and_then(|o| serde_json::from_str::<Value>(o).ok())
        .and_then(|v| {
            let c = v["context"].as_str()?.to_string();
            Some((
                c,
                v["contextReason"].as_str().unwrap_or_default().to_string(),
            ))
        });
    if let Some((c, _)) = &task_context {
        tail.push(Span::styled(
            format!("  {c} context"),
            Style::default().fg(t.link),
        ));
    }
    for (summary, _) in &notes {
        tail.push(Span::styled(
            format!("  {summary}"),
            Style::default().fg(t.link),
        ));
    }
    if tool.name == "shell" {
        tail.push(Span::styled(
            "  local · output not sent to the model",
            t.faint(),
        ));
    }
    // A question's time is the user's, not the tool's.
    let asks = matches!(tool.name.as_str(), "question" | "escalate");
    if tool.wait_from.is_some() && tool.duration.is_none() {
        tail.push(Span::styled(
            "  awaiting approval",
            Style::default().fg(t.yellow),
        ));
    } else if asks && tool.output.is_none() && tool.args["blocking"] != false {
        tail.push(Span::styled(
            "  waiting on you",
            Style::default().fg(t.yellow),
        ));
    } else if !tool.replayed && !asks {
        tail.push(Span::styled(
            format!("  {}", fmt_duration(duration)),
            t.faint(),
        ));
    }
    let diff_btn = diff.is_some();
    let btn_w = if diff_btn {
        2 + Btn::Diff.label().width()
    } else {
        0
    };
    let tail_w: usize = tail.iter().map(|s| s.content.width()).sum::<usize>() + btn_w;
    let head = format!("  {icon} ");
    let name = if tool.name == "shell" {
        "! ".to_string()
    } else {
        format!("{} ", tool.name)
    };
    let mut row = vec![
        Span::styled(head.clone(), Style::default().fg(color)),
        Span::styled(name.clone(), t.bold()),
    ];
    let mut used = head.width() + name.width();
    if tool.name == "task" {
        // The child, its live status, then the prompt: `build#1 running · prompt`.
        let (who, status) = match &tool.child {
            Some(c) => (
                format!(
                    "{}{} ",
                    c.label,
                    if c.follow_up { " follow-up" } else { "" }
                ),
                crate::engine::status_text(&c.status),
            ),
            None => (
                format!("{} ", tool.args["agent"].as_str().unwrap_or("agent")),
                if tool.output.is_none() {
                    "starting".into()
                } else {
                    String::new()
                },
            ),
        };
        let status_color = match status.as_str() {
            "done" => t.green,
            "roadblocked" => t.keyword,
            "failed" | "killed" => t.red,
            _ => t.yellow,
        };
        let status = if status.is_empty() {
            String::new()
        } else {
            format!("{status} · ")
        };
        used += who.width() + status.width();
        row.push(Span::styled(
            who,
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ));
        row.push(Span::styled(status, Style::default().fg(status_color)));
    }
    let room = width.saturating_sub(used + tail_w + 1);
    let key = truncate(&key_arg(tool), room);
    used += key.width();
    row.push(Span::styled(key, t.muted()));
    used += tail.iter().map(|s| s.content.width()).sum::<usize>();
    row.extend(tail);
    let mut meta = Meta::header(4);
    if diff_btn {
        row.push(Span::raw("  "));
        let x = used + 2;
        meta.buttons
            .push((x as u16, (x + Btn::Diff.label().width()) as u16, Btn::Diff));
        row.push(Span::styled(Btn::Diff.label(), Style::default().fg(t.dim)));
    }
    let mut out: Vec<Row> = vec![(Line::from(row), meta)];
    let gutter = Span::styled("    │ ", Style::default().fg(t.rule));
    let g = std::slice::from_ref(&gutter);
    // D120: the child's write scope and subscriptions, always visible.
    if tool.name == "task" {
        let list = |k: &str| -> Option<String> {
            let v = tool.args[k].as_array()?;
            Some(if v.is_empty() {
                "none".to_string()
            } else {
                v.iter()
                    .filter_map(|p| p.as_str())
                    .map(rel)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        };
        let mut parts = vec![];
        if let Some(sc) = list("scope") {
            parts.push(format!("writes {sc}"));
        }
        if let Some(sub) = list("subscribe") {
            parts.push(format!("watches {sub}"));
        }
        if !parts.is_empty() {
            let line = Line::from(Span::styled(parts.join(" · "), t.faint()));
            out.extend(wrap_rows(plain(vec![line], 0), width, g, g));
        }
    }
    let more_row = |text: String| -> Row {
        (
            Line::from(vec![gutter.clone(), Span::styled(text, t.faint())]),
            Meta {
                skip: true,
                header: true,
                ..Meta::default()
            },
        )
    };

    // Expanded bash and script rows show the whole command or code,
    // highlighted, above the output.
    let source = match tool.name.as_str() {
        "bash" | "shell" => tool.args["command"].as_str().map(|c| ("bash", c)),
        "script" => tool.args["code"].as_str().map(|c| ("python", c)),
        _ => None,
    };
    // A one-line command the header shows whole is not repeated.
    let source = source.filter(|(_, code)| {
        !matches!(tool.name.as_str(), "bash" | "shell")
            || code.trim().lines().count() > 1
            || key_arg(tool).width() > room
    });
    if expanded && let Some((lang, code)) = source {
        let mut hl = crate::highlight::Highlighter::new(lang);
        let lines: Vec<Line<'static>> = code
            .trim_end()
            .lines()
            .map(|l| Line::from(hl.line(&l.replace('\t', "    "), Style::default())))
            .collect();
        out.extend(wrap_rows(plain(lines, 0), width, g, g));
        out.push((
            Line::from(gutter.clone()),
            Meta {
                skip: true,
                ..Meta::default()
            },
        ));
    }
    if let Some(flag) = &tamper {
        let text = format!("! {flag}");
        let mark = Span::styled("    ", Style::default());
        let mut rows = wrap_rows(
            plain(
                vec![Line::from(Span::styled(
                    text,
                    Style::default().fg(t.yellow).add_modifier(Modifier::BOLD),
                ))],
                0,
            ),
            width,
            std::slice::from_ref(&mark),
            &[Span::raw("      ")],
        );
        if !expanded {
            rows.truncate(2);
        }
        out.extend(rows);
    }
    // D129: the whole output went to a file: its size and an open button.
    if let Some((bytes, lines, path)) = tool.output.as_deref().and_then(full_output) {
        let name = path
            .file_name()
            .map(|n| format!("outputs/{}", n.to_string_lossy()))
            .unwrap_or_else(|| path.display().to_string());
        let head = format!(
            "    ↳ full output · {} · {} line{} · ",
            fmt_bytes(bytes),
            lines,
            if lines == 1 { "" } else { "s" }
        );
        let label = Btn::Output.label();
        let room = width.saturating_sub(head.width() + label.width() + 2);
        let name = truncate(&name, room);
        let x = head.width() + name.width() + 2;
        let mut meta = Meta {
            skip: true,
            ..Meta::default()
        };
        meta.buttons
            .push((x as u16, (x + label.width()) as u16, Btn::Output));
        out.push((
            Line::from(vec![
                Span::styled(head, Style::default().fg(t.fog)),
                Span::styled(name, t.faint()),
                Span::raw("  "),
                Span::styled(label, Style::default().fg(t.link)),
            ]),
            meta,
        ));
    }
    if let Some(reason) = &tool.moved {
        out.push((
            Line::from(Span::styled(
                format!("    ↳ moved to background ({reason})"),
                Style::default().fg(t.fog),
            )),
            Meta {
                skip: true,
                ..Meta::default()
            },
        ));
    }
    if expanded && let Some((c, why)) = task_context.as_ref().filter(|(_, w)| !w.is_empty()) {
        out.extend(wrap_rows(
            plain(
                vec![Line::from(vec![
                    Span::styled(format!("{c} context — "), Style::default().fg(t.link)),
                    Span::styled(why.clone(), Style::default().fg(t.fog)),
                ])],
                0,
            ),
            width,
            g,
            g,
        ));
    }
    if expanded {
        for (summary, why) in notes.iter().filter(|(_, w)| !w.is_empty()) {
            out.extend(wrap_rows(
                plain(
                    vec![Line::from(vec![
                        Span::styled(format!("{summary} — judge: "), Style::default().fg(t.link)),
                        Span::styled(why.clone(), Style::default().fg(t.fog)),
                    ])],
                    0,
                ),
                width,
                g,
                g,
            ));
        }
    }
    if expanded
        && let Some(r) = &tool.review
        && let Some((_, verdict, reason)) = &r.result
    {
        let text = format!("safety review ({}): {verdict} — {}", r.model, reason.trim());
        out.extend(wrap_rows(
            plain(
                vec![Line::from(Span::styled(text, Style::default().fg(t.fog)))],
                0,
            ),
            width,
            g,
            g,
        ));
    }
    if !tool.inner.is_empty() {
        // Script inner calls, newest last; collapsed shows the latest few.
        let n = tool.inner.len();
        let limit = if expanded { n } else { 6 };
        if n > limit {
            out.push(more_row(format!("… {} earlier calls", n - limit)));
        }
        for (k, c) in tool.inner.iter().enumerate().skip(n - limit.min(n)) {
            let (icon, color) = match (&c.output, c.is_error) {
                (Some(_), false) => ("✓", t.green),
                (Some(_), true) => ("✗", t.red),
                (None, _) => (SPINNER[(tick + k) % SPINNER.len()], t.yellow),
            };
            let branch = if k + 1 == n { "└ " } else { "├ " };
            let head = format!("    {branch}");
            let tail = if c.waiting {
                Span::styled("  awaiting approval", Style::default().fg(t.yellow))
            } else {
                let d = c.duration.unwrap_or_else(|| c.started.elapsed());
                Span::styled(format!("  {}", fmt_duration(d)), t.faint())
            };
            let room =
                width.saturating_sub(head.width() + c.name.width() + 4 + tail.content.width());
            out.push((
                Line::from(vec![
                    Span::styled(head, Style::default().fg(t.rule)),
                    Span::styled(format!("{icon} "), Style::default().fg(color)),
                    Span::styled(format!("{} ", c.name), t.bold()),
                    Span::styled(
                        truncate(&call_arg(&c.name, &c.args).replace('\n', " "), room),
                        t.muted(),
                    ),
                    tail,
                ]),
                Meta::gutter(6),
            ));
            // A failed host call says why on the next row.
            if c.is_error
                && let Some(why) = c
                    .output
                    .as_deref()
                    .map(|o| o.strip_prefix("Error: ").unwrap_or(o))
                && let Some(first) = why.lines().find(|l| !l.trim().is_empty())
            {
                let bar = if k + 1 == n { "      " } else { "    │ " };
                let room = width.saturating_sub(bar.width() + 2);
                out.push((
                    Line::from(vec![
                        Span::styled(bar, Style::default().fg(t.rule)),
                        Span::styled(
                            format!("  {}", truncate(first.trim(), room)),
                            Style::default().fg(t.red),
                        ),
                    ]),
                    Meta::gutter(8),
                ));
            }
        }
    }
    if tool.name == "todo" {
        let items = todo_items(&tool.args["items"]);
        let limit = if expanded {
            usize::MAX
        } else {
            COLLAPSED_TODO_ROWS
        };
        let lines = items
            .iter()
            .take(limit)
            .map(|i| {
                let (mark, style) = match i.status.as_str() {
                    "completed" => (
                        "☑ ",
                        Style::default()
                            .fg(t.dim)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ),
                    "in_progress" => ("◐ ", Style::default().fg(t.yellow)),
                    _ => ("☐ ", t.text()),
                };
                Line::from(vec![
                    Span::styled(mark, style.remove_modifier(Modifier::CROSSED_OUT)),
                    Span::styled(i.content.clone(), style),
                ])
            })
            .collect();
        out.extend(wrap_rows(
            plain(lines, 0),
            width,
            g,
            &[Span::styled("    │   ", Style::default().fg(t.rule))],
        ));
    }
    if let Some(d) = diff {
        let total = d.lines.len();
        let limit = if expanded { total } else { COLLAPSED_DIFF_ROWS };
        let hang = [gutter.clone(), Span::raw(" ".repeat(d.indent))];
        out.extend(wrap_rows(
            plain(d.lines.into_iter().take(limit).collect(), d.indent),
            width,
            g,
            &hang,
        ));
        if total > limit {
            out.push(more_row(format!(
                "… {} more rows (click to expand)",
                total - limit
            )));
        }
    }
    let mut boxinfo = None;
    if tool.output.is_some() {
        let bash = match tool.name.as_str() {
            "bash" | "shell" => bash_display(body),
            "script" => script_display(body),
            _ => None,
        };
        let output = bash.as_deref().unwrap_or(body);
        // The ✗ and the red already say it failed.
        let output = if tool.is_error {
            output.strip_prefix("Error: ").unwrap_or(output)
        } else {
            output
        };
        let show_output = expanded || (tool.is_error && !matches!(tool.name.as_str(), "todo"));
        if show_output {
            let style = if tool.is_error {
                Style::default().fg(t.red)
            } else {
                Style::default().fg(t.fog)
            };
            let all: Vec<&str> = output.lines().collect();
            let limit = if expanded { MAX_OUTPUT_LINES } else { 3 };
            let lines = all
                .iter()
                .take(limit)
                .map(|l| Line::from(Span::styled(l.replace('\t', "    "), style)))
                .collect();
            let body_w = if expanded {
                width.saturating_sub(1)
            } else {
                width
            };
            let body = wrap_rows(plain(lines, 0), body_w, g, g);
            if expanded {
                boxinfo = boxed(
                    &mut out,
                    body,
                    RESULT_VISIBLE,
                    entry.box_scroll,
                    false,
                    &gutter,
                );
            } else {
                out.extend(body);
            }
            if all.len() > limit {
                out.push(more_row(format!("… {} more lines", all.len() - limit)));
            }
            if all.is_empty() && expanded {
                out.push(more_row("(no output)".into()));
            }
        }
    }
    (out, boxinfo)
}

fn render_pending(
    p: &Pending,
    scroll: Option<usize>,
    width: usize,
    tick: usize,
) -> (Vec<Row>, Option<BoxInfo>) {
    let t = theme();
    let name = if p.name.is_empty() { "tool" } else { &p.name };
    if p.abandoned.is_some() {
        let text = format!("  · {name} {} (abandoned)", p.key);
        return (
            vec![(
                Line::from(Span::styled(
                    truncate(&text, width),
                    Style::default()
                        .fg(t.dim)
                        .add_modifier(Modifier::CROSSED_OUT),
                )),
                Meta::gutter(4),
            )],
            None,
        );
    }
    let head = format!("  {} ", SPINNER[tick % SPINNER.len()]);
    let label = format!("writing {name}");
    let size = format!("  {}", fmt_chars(p.args.chars().count()));
    let room = width.saturating_sub(head.width() + label.width() + size.width() + 4);
    let key = if p.key.is_empty() {
        String::new()
    } else {
        format!(" · {}", truncate(&p.key, room))
    };
    let mut out = vec![(
        Line::from(vec![
            Span::styled(head, Style::default().fg(t.yellow)),
            Span::styled(
                label,
                Style::default().fg(t.fog).add_modifier(Modifier::BOLD),
            ),
            Span::styled(key, t.muted()),
            Span::styled(size, t.faint()),
        ]),
        Meta::gutter(4),
    )];
    let mut boxinfo = None;
    if let Some(preview) = p.preview.as_deref().filter(|s| !s.is_empty()) {
        let gutter = Span::styled("    ┊ ", Style::default().fg(t.rule));
        let lines = preview
            .lines()
            .map(|l| Line::from(Span::styled(l.replace('\t', "    "), t.faint())))
            .collect();
        let body = wrap_rows(
            plain(lines, 0),
            width.saturating_sub(1),
            std::slice::from_ref(&gutter),
            std::slice::from_ref(&gutter),
        );
        boxinfo = boxed(&mut out, body, PREVIEW_VISIBLE, scroll, true, &gutter);
    }
    (out, boxinfo)
}

/// The key field of a call being written, parsed tolerantly from partial JSON.
fn pending_key(name: &str, args: &str) -> String {
    let keys: &[&str] = match name {
        "read" | "write" | "edit" => &["path", "file_path"],
        "bash" => &["command"],
        "grep" | "glob" => &["pattern"],
        "task" => &["prompt", "continue"],
        "question" => &["text"],
        _ => &[
            "path",
            "file_path",
            "command",
            "pattern",
            "query",
            "url",
            "prompt",
        ],
    };
    let v = partial_field(args, keys).unwrap_or_default();
    rel(v.lines().next().unwrap_or_default())
}

/// The string value of the first of `keys` in a (possibly unterminated)
/// JSON object text, decoded as far as it goes.
pub fn partial_field(json: &str, keys: &[&str]) -> Option<String> {
    for key in keys {
        let pat = format!("\"{key}\"");
        let mut from = 0;
        while let Some(at) = json[from..].find(&pat) {
            let after = from + at + pat.len();
            let rest = json[after..].trim_start();
            if let Some(rest) = rest.strip_prefix(':') {
                let rest = rest.trim_start();
                if let Some(body) = rest.strip_prefix('"') {
                    return Some(decode_partial(body));
                }
                if rest.is_empty() {
                    return Some(String::new());
                }
            } else if rest.is_empty() {
                return Some(String::new());
            }
            from = after;
        }
    }
    None
}

fn decode_partial(body: &str) -> String {
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => {}
                Some('b') | Some('f') => {}
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

/// D129: `full output (N bytes, M lines): <path> — use read/grep on it`
/// in a tool result: (bytes, lines, path).
pub fn full_output(output: &str) -> Option<(u64, u64, std::path::PathBuf)> {
    let at = output.find("full output (")?;
    let rest = &output[at + "full output (".len()..];
    let (bytes, rest) = rest.split_once(" bytes, ")?;
    let (lines, rest) = rest.split_once(" line")?;
    let (_, rest) = rest.split_once("): ")?;
    let path = rest
        .split(" — ")
        .next()?
        .lines()
        .next()?
        .trim()
        .trim_matches('`');
    if path.is_empty() {
        return None;
    }
    Some((
        bytes.trim().parse().ok()?,
        lines.trim().parse().ok()?,
        std::path::PathBuf::from(path),
    ))
}

/// A byte count for people: `812 B`, `12.3 kB`, `4.0 MB`.
pub fn fmt_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1024 => format!("{:.1} kB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}
