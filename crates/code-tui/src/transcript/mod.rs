//! Transcript model: user messages, assistant markdown segments,
//! reasoning, compact tool rows (with diffs), pending (still being
//! written) tool calls, notices, and the engine events that build and
//! update them. Each [`Entry`] renders itself ([`entry`]); tool-call rows
//! are laid out in [`calls`]. Rendered rows carry [`Meta`] for selection,
//! copy buttons and toggles, and are cached per entry and width. Long
//! thinking and long tool results render inside an inner scroll box with
//! its own offset.
mod calls;
pub mod entry;

use crate::engine::{FileEdit, ToolCall, ToolResultData, ToolResultDetails};
use crate::rows::{Meta, Row};
use crate::theme::theme;
use calls::{partial_field, pending_key};
use entry::{Clip, Entry, Kind, Stream};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::time::{Duration, Instant};

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
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
    /// The user answered a question: a compact row, not a message.
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

/// A sidecar safety review of a call.
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
    /// `script` rows: inner host calls, oldest first.
    pub inner: Vec<Inner>,
    /// Successful write/edit results: the hunks with their line numbers.
    pub edits: Vec<FileEdit>,
    /// A steer or interrupt moved this foreground call to the background:
    /// the engine's reason.
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

#[derive(Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    /// Index of the first entry of the current model attempt; a stream reset
    /// discards assistant text and reasoning from there on.
    attempt_start: usize,
    show_reasoning: bool,
    /// Bumped whenever entries are removed or reordered (selections and
    /// anchors by index become invalid).
    pub generation: u64,
}

impl std::ops::Index<usize> for Transcript {
    type Output = Entry;
    fn index(&self, i: usize) -> &Entry {
        &self.entries[i]
    }
}

impl std::ops::IndexMut<usize> for Transcript {
    fn index_mut(&mut self, i: usize) -> &mut Entry {
        &mut self.entries[i]
    }
}

impl Transcript {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn entry(&self, i: usize) -> Option<&Entry> {
        self.entries.get(i)
    }
    pub fn entry_mut(&mut self, i: usize) -> Option<&mut Entry> {
        self.entries.get_mut(i)
    }
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
    /// The "older contexts on disk" row of a resumed session: `contexts`
    /// of `agent`, oldest first.
    pub fn push_older(&mut self, agent: &str, contexts: Vec<u32>) {
        self.entries.push(Entry::new(
            Kind::Older {
                agent: agent.to_string(),
                contexts,
                loading: false,
            },
            false,
        ));
    }
    /// A replayed earlier context window, collapsed.
    pub fn push_earlier(&mut self, earlier: Transcript) {
        self.entries
            .push(Entry::new(Kind::Earlier(earlier.entries), false));
    }
    /// For entry `i`: see [`Entry::decide_stamp`].
    fn decide_stamp(&mut self, i: usize) {
        if i >= self.entries.len() {
            return;
        }
        let (before, rest) = self.entries.split_at_mut(i);
        rest[0].decide_stamp(before);
    }
    /// Re-decide every entry's time (after a replay filled the times in).
    fn restamp(&mut self) {
        for i in 0..self.entries.len() {
            self.decide_stamp(i);
        }
    }
    /// The previous assistant segment is finished once anything follows it.
    fn seal_last(&mut self) {
        if let Some(e) = self.entries.last_mut() {
            e.seal();
        }
    }
    /// A model reply was committed: its text is finished, so the next
    /// reply's text starts a new paragraph.
    pub fn end_reply(&mut self) {
        self.seal_last();
    }
    /// Mark every assistant segment finished (turn end, replay) and the
    /// last segment of each turn as carrying the turn's copy row.
    pub fn seal(&mut self) {
        Entry::seal_all(&mut self.entries);
    }
    /// The markdown of every assistant segment in the turn that entry `i`
    /// ends, joined by blank lines.
    pub fn turn_markdown(&self, i: usize) -> String {
        let start = self.entries[..i]
            .iter()
            .rposition(Entry::is_user)
            .map_or(0, |u| u + 1);
        self.entries[start..=i]
            .iter()
            .filter_map(|e| e.assistant_text().map(str::trim))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    /// What the copy buttons of entry `i` copy: the turn's copy row copies
    /// the whole turn.
    pub fn clip(&self, i: usize) -> Option<Clip> {
        let e = self.entries.get(i)?;
        if e.ends_turn() {
            return Some(Clip::Markdown(self.turn_markdown(i)));
        }
        e.clip()
    }
    /// Whether the session has anything a resume would bring back.
    pub fn has_conversation(&self) -> bool {
        self.entries.iter().any(Entry::is_conversation)
    }
    /// The file edits of the turn since the last user message.
    pub fn turn_edits(&self) -> Vec<FileEdit> {
        let start = self.entries.iter().rposition(Entry::is_user).unwrap_or(0);
        self.entries[start..]
            .iter()
            .filter_map(|e| e.tool().filter(|t| !t.is_error).map(|t| t.edits.clone()))
            .flatten()
            .collect()
    }
    /// A compaction finished with `summary`: everything so far folds into
    /// an earlier context under a "compacted" divider. The summary closes
    /// the old context and heads the new one; false (and nothing changes)
    /// when it is that repeat.
    pub fn compacted(&mut self, summary: String) -> bool {
        if self
            .entries
            .last()
            .is_some_and(|e| e.is_compacted(&summary))
        {
            return false;
        }
        self.fold_earlier();
        self.push(Kind::Compacted(summary));
        true
    }
    /// How many user messages after entry `i` have exactly `text`.
    pub fn count_prompts_after(&self, i: usize, text: &str) -> usize {
        self.entries[i + 1..]
            .iter()
            .filter(|x| x.user_text() == Some(text))
            .count()
    }
    /// A finished call (fork chat): its live row gets the result, or a
    /// recorded row is added when it has none.
    pub fn tool_result(&mut self, call: ToolCall, result: ToolResultData) {
        let live = self
            .entries
            .iter()
            .any(|e| e.tool().is_some_and(|t| t.id == call.id));
        if live {
            self.tool_end(&call.id, result.output, result.is_error, result.details);
        } else {
            self.recorded_tool(call, Some(result));
        }
    }
    /// Open the newest row of call `id`.
    pub fn open_tool(&mut self, id: &str) {
        if let Some(e) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.tool().is_some_and(|t| t.id == id))
        {
            e.expand();
        }
    }
    /// In notices containing `marker`, replace `from` by `to`.
    pub fn retext_notices(&mut self, marker: &str, from: &str, to: &str) {
        for e in &mut self.entries {
            e.update_notice(|_, text| {
                if !text.contains(marker) {
                    return false;
                }
                *text = text.replace(from, to);
                true
            });
        }
    }
    /// Every (entry, row) whose text contains `query` (lowercase), at `width`.
    pub fn search(&mut self, query: &str, width: usize, tick: usize) -> Vec<(usize, usize)> {
        let mut matches = vec![];
        for (i, e) in self.entries.iter_mut().enumerate() {
            for j in e.matching_lines(query, width, tick) {
                matches.push((i, j));
            }
        }
        matches
    }
    /// Open entry `i` when it is a collapsed earlier context; true when it was.
    pub fn open_earlier(&mut self, i: usize) -> bool {
        self.entries.get_mut(i).is_some_and(Entry::open_earlier)
    }
    pub fn collapse(&mut self, i: usize) {
        if let Some(e) = self.entries.get_mut(i) {
            e.collapse();
        }
    }
    /// A loaded older context goes in just below the "on disk" row,
    /// above the contexts loaded before it.
    pub fn insert_earlier(&mut self, n: u32, earlier: Transcript) {
        let Some(empty) = self.entries.first_mut().and_then(|e| e.older_loaded(n)) else {
            return;
        };
        if !earlier.entries.is_empty() {
            self.entries
                .insert(1, Entry::new(Kind::Earlier(earlier.entries), false));
            if self.attempt_start >= 1 {
                self.attempt_start += 1;
            }
        }
        if empty {
            self.entries.remove(0);
            self.attempt_start = self.attempt_start.saturating_sub(1);
        }
        self.generation += 1;
    }
    /// The next older context to load, marking the row as loading.
    pub fn take_older(&mut self) -> Option<(String, u32)> {
        self.entries.first_mut()?.take_older()
    }
    /// Fold everything since the last fold into one collapsed "earlier
    /// context" section. The contexts folded before (and the "on disk" row)
    /// stay where they are, as headers of their own: each one still opens.
    fn fold_earlier(&mut self) {
        self.seal();
        let headers = self.entries.iter().take_while(|e| e.is_history()).count();
        let entries: Vec<Entry> = self
            .entries
            .split_off(headers)
            .into_iter()
            .filter(|e| !e.folds_away())
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
        self.entries
            .last_mut()
            .is_some_and(|e| e.append(Stream::Compacting, delta))
    }
    pub fn end_compacting(&mut self) {
        self.retain_entries(|e| !e.is_compacting());
    }
    pub fn begin_attempt(&mut self) {
        self.attempt_start = self.entries.len();
    }
    /// Give the last entry its journal time (replay).
    pub fn stamp_last(&mut self, rfc3339: &str) {
        if let Some(e) = self.entries.last_mut()
            && let Ok(t) = chrono::DateTime::parse_from_rfc3339(rfc3339)
        {
            e.set_time(t.with_timezone(&chrono::Local));
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
            && let Some(e) = self.entries.iter_mut().rev().find(|e| e.is_card(&card.key))
        {
            e.replace(Kind::Card(Box::new(card)));
            return;
        }
        self.push(Kind::Card(Box::new(card)));
    }
    pub fn append_text(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if self
            .entries
            .last_mut()
            .is_some_and(|e| e.append(Stream::Text, delta))
        {
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
        if self
            .entries
            .last_mut()
            .is_some_and(|e| e.append(Stream::Reasoning, delta))
        {
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
            if self.entries[i].is_streamed() {
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
        let found = self.entries.iter().rposition(|e| {
            e.pending().is_some_and(|p| {
                p.index == index
                    && p.abandoned.is_none()
                    && id.as_deref().is_none_or(|i| p.id.is_empty() || p.id == i)
            })
        });
        let at = match found {
            Some(at) => at,
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
                self.entries.len() - 1
            }
        };
        self.entries[at].update_pending(|p| {
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
            true
        });
    }
    /// Abandon every pending row (stream reset, retry, turn end).
    /// An interrupt stopped the stream: the text so far stays as a
    /// finished segment marked interrupted; tentative calls are dropped.
    pub fn interrupted(&mut self) {
        self.abandon_pending();
        if let Some(e) = self.entries.iter_mut().rev().find(|e| e.is_assistant()) {
            e.seal();
        }
        self.notice(Level::Warn, "interrupted · the partial reply above is kept");
        self.attempt_start = self.entries.len();
    }
    /// Keep a committed partial reply across resets of the next attempt.
    pub fn partial(&mut self, notice: impl Into<String>) {
        self.notice(Level::Warn, notice);
        self.abandon_pending();
        self.begin_attempt();
    }
    fn abandon_pending(&mut self) {
        for e in &mut self.entries {
            e.update_pending(|p| {
                if p.abandoned.is_some() {
                    return false;
                }
                p.abandoned = Some(Instant::now());
                true
            });
        }
    }
    /// Drop abandoned pending rows after a moment; true when any went.
    pub fn prune_abandoned(&mut self) -> bool {
        self.retain_entries(|e| {
            !e.pending()
                .is_some_and(|p| p.abandoned.is_some_and(|t| t.elapsed() >= ABANDONED_FOR))
        })
    }
    /// Preserve the attempt boundary when rows on either side are removed.
    fn retain_entries(&mut self, mut keep: impl FnMut(&Entry) -> bool) -> bool {
        let before = self.entries.len();
        let mut index = 0;
        let mut removed_before = 0;
        self.entries.retain(|e| {
            let retain = keep(e);
            if !retain && index < self.attempt_start {
                removed_before += 1;
            }
            index += 1;
            retain
        });
        self.attempt_start -= removed_before;
        let changed = self.entries.len() != before;
        if changed {
            self.generation += 1;
        }
        changed
    }
    pub fn has_abandoned(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.pending().is_some_and(|p| p.abandoned.is_some()))
    }
    /// A retry or reconnect: calls being written are abandoned, and the
    /// notice replaces the previous one when nothing came between, so a
    /// failing call shows one row counting its attempts.
    pub fn retry_notice(&mut self, text: &str) {
        self.abandon_pending();
        let replaced = self.entries.last_mut().is_some_and(|e| {
            e.update_notice(|level, t| {
                if level != Level::Warn
                    || !(t.starts_with("retry ") || t.starts_with("reconnecting · "))
                {
                    return false;
                }
                *t = text.to_string();
                true
            })
        });
        if !replaced {
            self.notice(Level::Warn, text.to_string());
        }
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
        let live = |e: &Entry, keep: &dyn Fn(&Pending) -> bool| {
            e.pending()
                .is_some_and(|p| p.abandoned.is_none() && keep(p))
        };
        let at = self
            .entries
            .iter()
            .position(|e| live(e, &|p| !p.id.is_empty() && p.id == id))
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|e| live(e, &|p| p.id.is_empty() || p.name == name))
            });
        match at {
            Some(i) => self.entries[i].replace(Kind::Tool(Box::new(tool))),
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
            .find(|e| e.tool().is_some_and(|t| t.id == row))
        {
            e.update_tool(|t| {
                t.wait_from.get_or_insert_with(Instant::now);
                if parent.is_some()
                    && let Some(c) = t.inner.iter_mut().rev().find(|c| c.id == call_id)
                {
                    c.waiting = true;
                }
                true
            });
        }
    }
    /// Approvals answered: waiting rows resume their run time.
    pub fn approval_done(&mut self) {
        for e in self.entries.iter_mut().rev() {
            e.update_tool(|t| {
                let Some(w) = t.wait_from.take() else {
                    return false;
                };
                t.waited += w.elapsed();
                t.inner.iter_mut().for_each(|c| c.waiting = false);
                true
            });
        }
    }
    pub fn tool_end(
        &mut self,
        id: &str,
        output: String,
        is_error: bool,
        details: ToolResultDetails,
    ) {
        if let Some(e) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.tool().is_some_and(|t| t.id == id))
        {
            e.update_tool(|t| {
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
                true
            });
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
    /// background as job `job`: from here on it is a background job
    /// row, as if it had been started with `background: true`.
    pub fn moved_to_background(&mut self, id: &str, job: &str, reason: &str) {
        if let Some(e) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.tool().is_some_and(|t| t.id == id))
        {
            e.update_tool(|t| {
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
                true
            });
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
        let Some(e) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.tool().is_some_and(|t| t.id == id))
        else {
            return false;
        };
        e.update_tool(|t| {
            t.review = Some(Review { model, result });
            true
        })
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
        let row = |keep: &dyn Fn(&Tool) -> bool| {
            self.entries
                .iter()
                .rposition(|e| e.tool().is_some_and(keep))
        };
        let at = parent
            .and_then(|p| row(&|t| t.id == p))
            .or_else(|| {
                job.and_then(|j| {
                    row(&|t| {
                        t.name == "script" && t.output.as_deref().is_some_and(|o| o.contains(j))
                    })
                })
            })
            .or_else(|| row(&|t| t.name == "script" && t.output.is_none()));
        let Some(i) = at else { return };
        self.entries[i].update_tool(|t| {
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
            true
        });
    }
    /// A turn ended (or was cancelled): calls that never finished are
    /// marked so, and the turn's segments are sealed.
    pub fn end_turn(&mut self) {
        self.abandon_running();
        self.seal();
    }
    /// A replay is complete: [`Transcript::end_turn`], then every time is
    /// decided from the replayed entries.
    pub fn end_replay(&mut self) {
        self.end_turn();
        self.restamp();
    }
    /// Mark tools that never finished (turn ended or was cancelled).
    fn abandon_running(&mut self) {
        self.abandon_pending();
        for e in &mut self.entries {
            e.update_tool(|t| {
                if t.output.is_some() {
                    return false;
                }
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
                true
            });
        }
    }
    pub fn set_show_reasoning(&mut self, show: bool) {
        self.show_reasoning = show;
        for e in &mut self.entries {
            e.reveal_reasoning(show);
        }
    }
    /// Flip "show reasoning"; the new setting.
    pub fn toggle_reasoning(&mut self) -> bool {
        let show = !self.show_reasoning;
        self.set_show_reasoning(show);
        show
    }
    /// A transcript that has nothing yet follows `other`'s "show
    /// reasoning" setting.
    pub fn adopt_reasoning(&mut self, other: &Transcript) {
        if self.entries.is_empty() {
            self.show_reasoning = other.show_reasoning;
        }
    }
    /// `task` rows whose link is missing or contradicted by the result:
    /// (entry index, child id hint, whether the row continues a child). The
    /// hint is the `continue` argument or the result's `agentId` — ids,
    /// never prompt text.
    pub fn unlinked_tasks(&self) -> Vec<(usize, Option<String>, bool)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e.tool() {
                Some(t) if t.name == "task" => {
                    let result = t
                        .output
                        .as_deref()
                        .and_then(|o| serde_json::from_str::<Value>(o).ok())
                        .and_then(|v| v["agentId"].as_str().map(str::to_string));
                    let hint = t.args["continue"].as_str().map(str::to_string).or(result);
                    match (&t.child, &hint) {
                        (None, _) => Some((i, hint, e.continues_task())),
                        // A provisional link the result contradicts.
                        (Some(c), Some(h)) if c.id != *h => Some((i, hint, e.continues_task())),
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
    /// Never matched on prompt text: identical prompts are common. With
    /// whether the row continues a child.
    pub fn task_row_for(&self, agent: &str, call_id: Option<&str>) -> Option<(usize, bool)> {
        let row = self.task_row(agent, call_id)?;
        Some((row, self.entries[row].continues_task()))
    }
    fn task_row(&self, agent: &str, call_id: Option<&str>) -> Option<usize> {
        fn running_task(e: &Entry) -> Option<&Tool> {
            e.tool().filter(|t| t.name == "task" && t.output.is_none())
        }
        if let Some(call) = call_id {
            return self
                .entries
                .iter()
                .position(|e| e.tool().is_some_and(|t| t.id == call));
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
        if let Some(e) = self.entries.get_mut(i) {
            e.update_tool(|t| {
                t.child = Some(link);
                true
            });
        }
    }
    /// Refresh the status shown on `task` rows linked to `id`.
    pub fn child_status(&mut self, id: &str, status: &str) {
        for e in &mut self.entries {
            e.update_tool(|t| match &mut t.child {
                Some(c) if c.id == id && c.status != status => {
                    c.status = status.to_string();
                    true
                }
                _ => false,
            });
        }
    }
    /// Whether a blank separator row precedes entry `i`.
    pub fn gap_before(&self, i: usize) -> bool {
        i > 0 && self.entries[i].separated_from(&self.entries[i - 1])
    }
    /// Indices of user messages, oldest first.
    pub fn user_entries(&self) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| self.entries[i].is_user())
            .collect()
    }
    /// Anchor one user message (None clears).
    pub fn set_anchor(&mut self, i: Option<usize>) {
        for (j, e) in self.entries.iter_mut().enumerate() {
            e.set_anchored(Some(j) == i);
        }
    }
    pub fn anchor(&self) -> Option<usize> {
        self.entries.iter().position(Entry::is_anchored)
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

#[cfg(test)]
mod tests {
    use super::*;

    impl Transcript {
        /// The kinds of all entries, oldest first.
        pub(crate) fn kinds(&self) -> impl Iterator<Item = &Kind> {
            self.entries.iter().map(Entry::kind)
        }
    }

    #[test]
    fn pruning_abandoned_calls_preserves_partial_reply_boundary() {
        let mut tr = Transcript::default();
        tr.begin_attempt();
        tr.append_text("committed partial");
        tr.tool_delta(0, None, Some("write".into()), "{");
        tr.partial("output limit");
        tr.append_text("uncommitted retry");
        tr.tool_delta(1, None, Some("read".into()), "{");
        tr.abandon_pending();
        for e in &mut tr.entries {
            e.update_pending(|p| {
                p.abandoned = Some(Instant::now() - ABANDONED_FOR);
                true
            });
        }
        let generation = tr.generation;
        assert!(tr.prune_abandoned());
        assert_eq!(tr.generation, generation + 1);
        assert_eq!(tr.attempt_start, 2);
        tr.append_reasoning("failed reasoning");
        tr.stream_reset();
        assert_eq!(tr.entries.len(), 2);
        assert!(
            matches!(tr.entries[0].kind(), Kind::Assistant(text) if text == "committed partial")
        );
        assert!(
            matches!(tr.entries[1].kind(), Kind::Notice(Level::Warn, text) if text == "output limit")
        );
        assert!(!tr.prune_abandoned());
    }

    #[test]
    fn removing_compaction_rows_preserves_attempt_boundary() {
        let mut tr = Transcript::default();
        tr.append_text("committed reply");
        tr.compacting();
        tr.begin_attempt();
        tr.append_text("failed retry");
        tr.compacting();
        tr.end_compacting();
        assert_eq!(tr.attempt_start, 1);
        tr.stream_reset();
        assert_eq!(tr.entries.len(), 1);
        assert!(matches!(tr.entries[0].kind(), Kind::Assistant(text) if text == "committed reply"));
    }

    fn texts(tr: &Transcript) -> Vec<&str> {
        tr.kinds()
            .filter_map(|k| match k {
                Kind::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn consecutive_replies_are_separate_paragraphs() {
        let mut tr = Transcript::default();
        tr.push(Kind::User("go".into()));
        for _ in 0..3 {
            tr.append_text("done.");
            tr.end_reply();
        }
        assert_eq!(texts(&tr), ["done.", "done.", "done."]);
        assert!((2..=3).all(|i| tr.gap_before(i)));
        // One reply streamed in pieces stays one paragraph.
        tr.append_text("one ");
        tr.append_text("reply");
        assert_eq!(texts(&tr).last(), Some(&"one reply"));
    }

    #[test]
    fn each_compaction_adds_an_earlier_context_that_opens() {
        let mut tr = Transcript::default();
        tr.push(Kind::Older {
            agent: "root".into(),
            contexts: vec![1],
            loading: false,
        });
        for round in ["first", "second"] {
            tr.push(Kind::User(round.into()));
            tr.append_text("reply");
            tr.end_reply();
            tr.compacted(format!("summary {round}"));
        }
        let top: Vec<&Kind> = tr.kinds().collect();
        assert!(matches!(top[0], Kind::Older { .. }));
        let earlier: Vec<usize> = (0..top.len())
            .filter(|&i| matches!(top[i], Kind::Earlier(_)))
            .collect();
        // Two folds side by side, neither inside the other.
        assert_eq!(earlier, [1, 2]);
        for &i in &earlier {
            let Kind::Earlier(children) = top[i] else {
                unreachable!()
            };
            assert!(
                !children
                    .iter()
                    .any(|c| matches!(c.kind(), Kind::Earlier(_)))
            );
        }
        for i in earlier {
            assert!(tr.entry_mut(i).unwrap().toggle());
        }
        // The on-disk row still leads, so loading the older context works.
        tr.insert_earlier(
            1,
            Transcript {
                entries: vec![Entry::new(Kind::Assistant("old".into()), false)],
                ..Transcript::default()
            },
        );
        assert_eq!(
            tr.kinds().filter(|k| matches!(k, Kind::Earlier(_))).count(),
            3
        );
    }

    #[test]
    fn loading_earlier_context_preserves_attempt_boundary() {
        for empty in [false, true] {
            for started in [false, true] {
                let mut tr = Transcript::default();
                tr.push(Kind::Older {
                    agent: "root".into(),
                    contexts: if empty { vec![1] } else { vec![1, 2] },
                    loading: true,
                });
                if started {
                    tr.begin_attempt();
                }
                tr.append_text("failed retry");
                tr.insert_earlier(
                    1,
                    Transcript {
                        entries: vec![Entry::new(Kind::Assistant("old reply".into()), false)],
                        ..Transcript::default()
                    },
                );
                assert_eq!(
                    tr.attempt_start,
                    if started {
                        if empty { 1 } else { 2 }
                    } else {
                        0
                    }
                );
                tr.stream_reset();
                assert!(!tr.kinds().any(|k| matches!(k, Kind::Assistant(_))));
                assert!(tr.kinds().any(|k| matches!(k, Kind::Earlier(_))));
            }
        }
    }

    #[test]
    fn partial_reply_survives_later_stream_reset() {
        let mut tr = Transcript::default();
        tr.begin_attempt();
        tr.append_text("committed partial");
        tr.tool_delta(0, None, Some("write".into()), "{");
        tr.partial("output limit");
        assert!(
            tr.kinds()
                .any(|k| matches!(k, Kind::Pending(p) if p.abandoned.is_some()))
        );
        tr.append_text("uncommitted retry");
        tr.stream_reset();
        assert!(
            tr.kinds()
                .any(|k| matches!(k, Kind::Assistant(text) if text == "committed partial"))
        );
        assert!(
            !tr.kinds()
                .any(|k| matches!(k, Kind::Assistant(text) if text == "uncommitted retry"))
        );
    }
}
