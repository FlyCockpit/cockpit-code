//! Application state, input handling and slash commands. Drawing lives in
//! `view`, pointer handling and copy in `mouse`.
mod attention;
mod engine_o;
mod goal;
mod layout;
mod mention;
mod mouse;
mod panels;
mod queue;
mod search;
mod shell;
mod terminals;
mod tree_rows;
mod usage_view;
mod view;

use crate::agents::{Tree, job_age};
use crate::clipboard::Clipboard;
use crate::composer::Composer;
use crate::dialog::{Body, Dialog, DialogOption, Outcome};
use crate::engine::{
    ApprovalChoice, ApprovalView, Catalog, ContractView, Engine, EventRx, GoalView, LockInfo, Mode,
    OpenError, QuestionView, ReplayItem, Target as EngineTarget, UiEvent, Usage, alive, fmt_cost,
};
use crate::forkchat::ForkChat;
use crate::list::{self, ListPicker};
use crate::pager::{self, Pager};
use crate::picker::{self, Picker, fmt_tokens};
use crate::questions::{self, QDialog};
use crate::theme::theme;
use crate::transcript::{Level, Transcript, entry::Kind};
use crate::ui::{self, Hits, ScrollId, StatusSeg, Target};
use crate::wrap::{truncate, wrap_lines};
use goal::{EditPurpose, GoalAction, GoalPrompt};
use layout::Layout;
use mouse::Press;
use ratatui::Frame;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};
use terminals::Terms;
pub use terminals::recv_term;
use usage_view::{UsageData, UsageView};
use view::{CostView, Focus, Menu, Sel};

const COMMANDS: &[(&str, &str)] = &[
    ("model", "pick the model; toggle subagent availability"),
    ("effort", "pick the reasoning effort"),
    ("permissions", "pick the permission mode"),
    ("compact", "summarize the context: /compact [focus]"),
    ("resume", "switch to another session"),
    ("fork", "fork this session here or at a message"),
    ("new", "start a new session"),
    (
        "goal",
        "work autonomously to a contract: /goal <text> · pause · resume · edit · clear",
    ),
    ("criteria", "the viewed agent's criteria checklist"),
    (
        "agents",
        "the agent tree: filter, open, ask status, finalize, stop (ctrl+t: strip)",
    ),
    (
        "decisions",
        "decisions agents made on your behalf: keep, change, discuss",
    ),
    (
        "subagents",
        "tree or flat subagents for this session (before the first message)",
    ),
    ("jobs", "background jobs: view output, kill"),
    (
        "terminal",
        "show or hide the terminal sidebar (ctrl+] types in it)",
    ),
    ("mcp", "MCP servers: enable, connection state, tools"),
    ("answer", "reply to pending questions and decisions"),
    (
        "guidance",
        "what contextual guidance was injected, per agent, and why",
    ),
    (
        "prune",
        "stub old delegation prompts and tool outputs (breaks the prompt cache)",
    ),
    ("cost", "cost and tokens by model and agent"),
    (
        "btw",
        "side chat with the viewed agent (a hidden fork): /btw [question]",
    ),
    ("diff", "changes: worktree, staged, this turn's edits"),
    ("usage", "limits, resets, credits and budgets by provider"),
    ("copy", "copy the chat selection (also ctrl+shift+c)"),
    ("mouse", "toggle mouse capture (also F12)"),
    ("vim", "toggle vim keys in the composer for this session"),
    (
        "exclusive",
        "one model for everything: /exclusive [model] · /exclusive off",
    ),
    ("rename", "set the session title: /rename <title>"),
    ("help", "keys and commands"),
    ("quit", "exit cockpit-code"),
    ("exit", "exit cockpit-code"),
];
/// "retry 3 in 1.6s · connection refused": one row per failing call,
/// updated in place by each attempt.
/// The reconnect row (updated in place while the network is down).
fn reconnect_text(attempt: usize, delay_ms: u64) -> String {
    format!(
        "reconnecting · attempt {attempt} · next try in {:.0}s · the network dropped; this continues by itself (esc cancels)",
        (delay_ms as f64 / 1000.0).max(1.0)
    )
}

/// A reply that stopped early: an output limit or a provider
/// pause keeps the text and continues; anything else was discarded.
fn partial_text(status: &str, text: &str) -> String {
    let words = text.split_whitespace().count();
    match status {
        "max_tokens" => format!(
            "reply hit the output limit after {words} words · its text is kept and the model continues; tool calls from that reply were not run"
        ),
        "pause_turn" => "the provider paused the turn · continuing automatically".into(),
        _ => format!(
            "reply ended with {status}; its {words}-word partial output and any tool calls were discarded"
        ),
    }
}

/// A backup switch, live and replayed.
fn switch_text(from: &str, to: &str, reason: &str) -> String {
    let reason = reason.lines().next().unwrap_or_default();
    format!(
        "switched to backup {to} for this turn (from {from}) · {}",
        truncate(reason, 100)
    )
}

fn retry_text(attempt: usize, delay_ms: u64, reason: Option<&str>) -> String {
    let base = format!("retry {attempt} in {:.1}s", delay_ms as f64 / 1000.0);
    match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => format!("{base} · {}", truncate(r, 100)),
        None => base,
    }
}

/// A session grant key (`bash:git:status`, `web:fetch:host`,
/// `path:edit:/dir`, `mcp:server:tool`) in words.
fn grant_label(key: &str) -> String {
    let parts: Vec<&str> = key.splitn(3, ':').collect();
    match parts.as_slice() {
        ["bash", prog, ""] => format!("bash commands that run {prog}"),
        ["bash", prog, sub] => format!("bash commands that run {prog} {sub}"),
        ["web", "search"] => "web searches".into(),
        ["web", "fetch", host] => format!("fetches from {host}"),
        ["mcp", server, tool] => format!("{server} · {tool}"),
        ["path", tool, dir] => format!("{tool} under {dir}"),
        _ => key.to_string(),
    }
}
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_COMPOSER_ROWS: usize = 10;
/// Side column for the criteria panel.
const SIDE_W: u16 = 40;
/// Below this width the criteria panel is hidden.
const WIDE: u16 = 100;

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Waiting,
    Thinking,
    Writing,
    Tool,
    Retrying,
    Reconnecting,
    Compacting,
    Cancelling,
}

#[derive(Default)]
struct ModelStats {
    calls: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    known_cost: f64,
    known_calls: u64,
    unknown_calls: u64,
}

enum DialogPurpose {
    Quit,
    Effort,
    Mode,
    Fork,
    Lock(LockInfo),
    Approval(String),
    /// Switch sessions although work is running; text to refill the composer.
    Switch(EngineTarget, String, Option<String>),
    /// Cache-aware model switch confirmation: the new model.
    ModelSwitch(String),
    /// `/exclusive` step 1: use a safety model? (the model)
    ExclusiveSafety(String),
    /// `/exclusive` step 2: also block web tools and remote MCP servers?
    ExclusiveBlock(String, Option<String>),
    /// The `/goal` draft approval.
    GoalDraft,
    /// A criteria proposal: (agent, proposal id).
    Proposal(String, String),
    /// A root roadblock: its id.
    Roadblock(String),
    /// The `/goal` status dialog.
    GoalStatus,
    /// Trust project reviewer guidance: its request id.
    Guidance(String),
    GoalClear,
    /// `/resume`: rename or delete that session (its id).
    RenameSession(String),
    DeleteSession(String),
    /// A scope request root owns: its id.
    Scope(String),
    /// `/prune` confirmation.
    Prune,
    /// A read-only viewer (`/guidance`).
    Viewer,
    /// A dialog opened from a panel (`/agents`, `/decisions`, `/subagents`).
    Panel(panels::PanelDialog),
    /// Close a private terminal with an optional note: its id.
    TerminalClose(String),
}

enum ListPurpose {
    Resume,
    ForkAt(Vec<(u32, usize, String)>),
    Jobs,
    Mcp,
}

enum Overlay {
    None,
    Dialog(Box<Dialog>, DialogPurpose),
    Picker(Box<Picker>),
    List(Box<ListPicker>, ListPurpose),
    /// `/diff`: worktree, staged, this turn.
    Diff(Box<crate::diffview::DiffView>),
    Pager(Box<Pager>),
    Cost(CostView),
    Usage(UsageView),
    Menu(Box<Menu>),
    /// One or several `question` calls.
    Questions(Box<QDialog>),
    /// "Chat about this in a fork".
    ForkChat(Box<ForkChat>),
    /// `/criteria`.
    Criteria(Box<crate::criteria::CriteriaView>),
    /// The contract editor.
    Contract(Box<crate::criteria::ContractEditor>, EditPurpose),
    /// `/agents`.
    Agents(Box<crate::agentsview::AgentsView>),
    /// `/decisions`.
    Decisions(Box<crate::decisionsview::DecisionsView>),
}

enum Prompt {
    Approval(ApprovalView),
    /// A question, and whether the user put it off (`/answer` reopens it).
    Question(QuestionView, bool),
    /// A goal draft, proposal or roadblock, and whether it was put off.
    Goal(GoalPrompt, bool),
    /// Project reviewer guidance to trust, and whether put off.
    Guidance(crate::engine::GuidanceView, bool),
    /// A scope request root owns, and whether put off.
    Scope(crate::engine::ScopeView, bool),
}

pub struct App {
    /// Errors from background engine calls (scope answers).
    async_errors: (
        std::sync::mpsc::Sender<String>,
        std::sync::mpsc::Receiver<String>,
    ),
    /// The backup model serving this turn, if one took over.
    backup_model: Option<String>,
    /// The `/resume` filter, kept across its rename/delete dialogs.
    resume_filter: String,
    /// The `@` completion popup.
    mentions: mention::Mentions,
    /// The Ctrl+F find bar, while open.
    search: Option<search::Search>,
    /// URLs visible in the last frame (OSC 8, clicks).
    links: Vec<crate::links::Link>,
    shells: Vec<shell::Shell>,
    shell_seq: u64,
    attention: attention::Attention,
    git: crate::git::GitWatch,
    engine: Engine,
    catalog: Catalog,
    transcript: Transcript,
    composer: Composer,
    events: Option<EventRx>,
    running: bool,
    compacting: bool,
    phase: Phase,
    turn_started: Instant,
    user_cancelled: bool,
    // ---- queue box ----
    /// The queued message with keyboard focus.
    queue_focus: Option<u64>,
    queue_scroll: usize,
    queue_area: Rect,
    /// A queued message taken into the composer for editing, and its place.
    queue_edit: Option<(crate::engine::Queued, usize)>,
    stats: BTreeMap<String, ModelStats>,
    ctx: (u64, Option<u64>),
    warned: bool,
    prompts: VecDeque<Prompt>,
    /// Title of the open approval/question dialog, before the "N more waiting" suffix.
    prompt_title: String,
    read_only: Option<String>,
    /// Root's criteria contract (status strip, side panel).
    contract: ContractView,
    /// The `/goal` run, unless there is none or it was cleared.
    goal: Option<GoalView>,
    /// A goal action waiting for root to be idle.
    deferred_goal: Option<GoalAction>,
    /// An older context being read for scroll-back: agent, number.
    older_job: Option<(String, u32, std::sync::mpsc::Receiver<Vec<ReplayItem>>)>,
    /// Skills invocable as slash commands, read at open and turn end.
    skills: Vec<crate::engine::SkillView>,
    /// The last keystroke (typing restarts the idle timer).
    last_key: Instant,
    /// The goal state last announced in the transcript.
    goal_announced: Option<String>,
    /// The criteria side panel (the viewed agent's).
    show_goals: bool,
    list_all: bool,
    /// Top transcript row; None follows the bottom.
    scroll: Option<usize>,
    overlay: Overlay,
    /// The overlay a pager was opened from (a finding location in
    /// `/criteria`, an output in `/agents`); Esc returns to it.
    overlay_back: Option<Overlay>,
    /// A fork chat set aside while its own web-tool approval is shown;
    /// it comes back when no fork-chat approval is left.
    parked_fork: Option<Box<ForkChat>>,
    /// Fork-chat snapshots (`opened`) the engine queued and the view has
    /// not received yet, per fork: one per successful open call.
    queued_snapshots: BTreeMap<String, usize>,
    slash_sel: usize,
    /// The wheel's top row of the slash / `@` popups; None follows
    /// the selection. Any key resets them.
    slash_top: Option<usize>,
    mention_top: Option<usize>,
    slash_dismissed: bool,
    ctrl_c_at: Option<Instant>,
    hint: Option<(String, Instant)>,
    pub mouse: bool,
    mouse_changed: bool,
    tick: usize,
    pub quit: bool,
    dirty: bool,
    transcript_area: Rect,
    tree: Tree,
    /// The subagent whose transcript is open; None is root.
    view: Option<String>,
    wide: bool,
    // ---- pointer, hover, selection ----
    hits: Hits,
    mouse_pos: Option<(u16, u16)>,
    hover: Option<(Rect, Target)>,
    /// The transcript entry under the pointer at the last draw (gap rows
    /// count as the entry above): its hover-only copy row.
    hover_entry: Option<usize>,
    press: Option<Press>,
    sel: Option<Sel>,
    /// A scrollbar thumb being dragged, and the grab row within the thumb.
    thumb_drag: Option<(ScrollId, isize)>,
    layout: Layout,
    /// Scroll so that entry `.0` row `.1` lands at transcript row `.2`
    /// (toggle anchoring, jumps); resolved at the next draw.
    anchor_req: Option<(usize, usize, isize)>,
    focus: Option<Focus>,
    clip: Clipboard,
    sticky: Option<usize>,
    /// (view, transcript generation) the selection and focus belong to.
    sel_owner: (Option<String>, u64),
    // ---- agent strip ----
    strip_focus: bool,
    strip_sel: usize,
    strip_scroll: usize,
    strip_area: Rect,
    show_finished: bool,
    goals_area: Rect,
    goals_scroll: usize,
    comp_area: Rect,
    comp_skip: usize,
    // ---- model switch, context ----
    /// Compact with the current model, then switch to this one.
    pending_switch: Option<String>,
    /// The context meter is an estimate (resume without usage).
    ctx_estimate: bool,
    /// The provider reports no context (meter source `unknown`): `ctx ?`.
    ctx_unknown: bool,
    /// A pending `$EDITOR` run, serviced by the event loop.
    editor: Option<EditorReq>,
    /// Composer chip rects from the last draw: (rect, block index).
    chip_rects: Vec<(Rect, usize)>,
    /// The session title (auto or `/rename`).
    title: Option<String>,
    /// The model picker is choosing for `/exclusive`: None the
    /// exclusive model, Some(model) its safety model.
    exclusive_pick: Option<Option<String>>,
    /// The startup MCP health check runs until this instant.
    mcp_check_until: Option<Instant>,
    /// Servers the health check reported (shown once; marked in /mcp).
    pub(super) mcp_problems: BTreeMap<String, String>,
    // ---- usage, quota ----
    /// The latest live quota report per provider (`UsageRecord.quota`).
    quotas: BTreeMap<String, crate::usage::Limit>,
    /// The latest background usage read.
    usage: Vec<crate::usage::ProviderUsage>,
    /// Live `agents finalize` progress.
    finalizing: Vec<panels::Finalizing>,
    /// Mail that arrived while root was idle: why its next turn started.
    wake_reasons: Vec<String>,
    /// Open decisions (status bar).
    decision_count: usize,
    /// Root mail held by a user cancel.
    mail_held: bool,
    /// Finished `agents ask` / `stop` calls.
    agent_done: (
        std::sync::mpsc::Sender<crate::engine::AgentDone>,
        std::sync::mpsc::Receiver<crate::engine::AgentDone>,
    ),
    /// The terminal sidebar.
    terms: Terms,
}

/// Text to edit in `$VISUAL`/`$EDITOR` with the TUI suspended.
pub struct EditorReq {
    pub text: String,
    target: EditTarget,
}

enum EditTarget {
    /// The whole composer, chips expanded between delimiters.
    Composer,
    /// One composer chip (block index, paste id).
    Chip(usize, String),
    /// A read-only copy (a transcript paste).
    View,
    /// The goal contract as YAML, and the draft before editing.
    Contract(EditPurpose, Box<crate::engine::ContractDraft>),
}

pub async fn recv(rx: &mut Option<EventRx>) -> Option<UiEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

fn mode_color(mode: Mode) -> Color {
    let t = theme();
    match mode {
        Mode::Yolo => t.red,
        Mode::Unattended => t.yellow,
        Mode::Auto => t.link,
        Mode::Manual => t.green,
    }
}

fn short(id: &str) -> &str {
    &id[..id.char_indices().nth(8).map_or(id.len(), |(at, _)| at)]
}

impl App {
    pub fn new(engine: Engine, events: EventRx, issue: Option<OpenError>) -> Self {
        let catalog = engine.catalog();
        crate::wrap::set_cwd(engine.cwd());
        let mut app = Self {
            async_errors: std::sync::mpsc::channel(),
            backup_model: None,
            resume_filter: String::new(),
            mentions: Default::default(),
            search: None,
            links: vec![],
            shells: vec![],
            shell_seq: 0,
            attention: Default::default(),
            git: crate::git::GitWatch::new(engine.cwd()),
            mouse: catalog.mouse,
            engine,
            catalog,
            transcript: Transcript::default(),
            composer: Composer::default(),
            events: None,
            running: false,
            compacting: false,
            phase: Phase::Idle,
            turn_started: Instant::now(),
            user_cancelled: false,
            queue_focus: None,
            queue_scroll: 0,
            queue_area: Rect::default(),
            queue_edit: None,
            stats: BTreeMap::new(),
            ctx: (0, None),
            warned: false,
            prompts: VecDeque::new(),
            prompt_title: String::new(),
            read_only: None,
            contract: ContractView::default(),
            goal: None,
            deferred_goal: None,
            goal_announced: None,
            last_key: Instant::now(),
            older_job: None,
            skills: vec![],
            show_goals: false,
            list_all: false,
            scroll: None,
            overlay: Overlay::None,
            overlay_back: None,
            parked_fork: None,
            queued_snapshots: BTreeMap::new(),
            slash_sel: 0,
            slash_top: None,
            mention_top: None,
            slash_dismissed: false,
            ctrl_c_at: None,
            hint: None,
            mouse_changed: false,
            tick: 0,
            quit: false,
            dirty: true,
            transcript_area: Rect::default(),
            tree: Tree::default(),
            view: None,
            wide: true,
            hits: Hits::default(),
            mouse_pos: None,
            hover: None,
            hover_entry: None,
            press: None,
            sel: None,
            thumb_drag: None,
            layout: Layout::default(),
            anchor_req: None,
            focus: None,
            clip: Clipboard::detect(),
            sticky: None,
            sel_owner: (None, 0),
            strip_focus: false,
            strip_sel: 0,
            strip_scroll: 0,
            strip_area: Rect::default(),
            show_finished: false,
            goals_area: Rect::default(),
            goals_scroll: 0,
            comp_area: Rect::default(),
            comp_skip: 0,
            pending_switch: None,
            ctx_estimate: false,
            ctx_unknown: false,
            editor: None,
            chip_rects: vec![],
            title: None,
            exclusive_pick: None,
            mcp_check_until: Some(Instant::now() + Duration::from_secs(15)),
            mcp_problems: BTreeMap::new(),
            quotas: BTreeMap::new(),
            usage: vec![],
            finalizing: vec![],
            wake_reasons: vec![],
            decision_count: 0,
            mail_held: false,
            agent_done: std::sync::mpsc::channel(),
            terms: Terms::default(),
        };
        if app.catalog.vim {
            app.composer.enable_vim();
        }
        let deferred = app.engine.deferred_model.take();
        app.load_session(events, "session");
        match issue {
            Some(OpenError::Locked(info)) => app.lock_dialog(info),
            Some(OpenError::Other(e)) => app.transcript.notice(Level::Error, e),
            None => {}
        }
        // `--model` on resume goes through the cache-aware confirmation.
        if let Some(id) = deferred {
            app.activate_model(id);
        }
        // The engine's usage cache started with the process.
        app.usage = app.engine.provider_usage();
        app
    }

    /// Show the project's reviewer guidance and ask for trust.
    fn guidance_dialog(&mut self, g: &crate::engine::GuidanceView) {
        let rel = g
            .path
            .strip_prefix(self.engine.cwd())
            .unwrap_or(&g.path)
            .display()
            .to_string();
        let sha = &g.sha256[..g.sha256.len().min(12)];
        let (title, md, trust, trust_why, decline_why) = if g.tampered {
            // The file already holds the restored, approved bytes; the
            // proposal is quarantined until the user approves it.
            let restored = std::fs::read_to_string(&g.path).unwrap_or_default();
            let change = if g.deleted {
                format!(
                    "An agent **deleted** `{rel}` without your approval. The approved file was restored.\n\nApprove the deletion?"
                )
            } else {
                let diff = similar::TextDiff::configure()
                    .algorithm(similar::Algorithm::RawMyers)
                    .diff_lines(&restored, &g.content)
                    .unified_diff()
                    .context_radius(3)
                    .header(&format!("approved/{rel}"), &format!("proposed/{rel}"))
                    .to_string();
                format!(
                    "An agent **changed** `{rel}` without your approval. The approved content was restored; the change below (sha256 `{sha}`) applies only if you approve it.\n\n```diff\n{}\n```",
                    diff.trim_end()
                )
            };
            (
                "Reviewer guidance was tampered with",
                change,
                if g.deleted {
                    "Approve the deletion"
                } else {
                    "Approve the change"
                },
                if g.deleted {
                    "the file is removed"
                } else {
                    "the reviewer uses the new content"
                },
                "keep the restored content",
            )
        } else if g.library {
            // A project guidance library file, not reviewer guidance.
            (
                "Trust project guidance library?",
                format!(
                    "This project adds items to your contextual guidance library from `{rel}` (sha256 `{sha}`). After an agent writes, a selector may append these items to its tool results, so a cloned repo or an agent could use the file to steer agents. It applies only once you trust this exact content; any change asks again.\n\n```markdown\n{}\n```",
                    g.content.trim_end()
                ),
                "Trust this content",
                "its items can be injected after writes in this project",
                "guidance runs without this file",
            )
        } else {
            (
                "Trust project reviewer guidance?",
                format!(
                    "This project asks the safety reviewer to follow extra guidance from `{rel}` (sha256 `{sha}`). A cloned repo or an agent could use it to loosen reviews, so it applies only once you trust this exact content; any change asks again.\n\n```markdown\n{}\n```",
                    g.content.trim_end()
                ),
                "Trust this content",
                "appended to the reviewer prompt for this project",
                "reviews run without it",
            )
        };
        let dialog = Dialog::new(
            title,
            Body::Markdown(md),
            vec![
                DialogOption::new("trust", trust)
                    .key('t')
                    .guarded()
                    .describe(trust_why),
                DialogOption::new(
                    "decline",
                    if g.tampered {
                        "Keep restored"
                    } else {
                        "Don't trust"
                    },
                )
                .key('d')
                .describe(decline_why),
            ],
        )
        .select("decline");
        self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Guidance(g.id.clone()));
    }

    /// Background discovery finished: new catalog; newly failed providers
    /// get one notice each.
    fn discovery_updated(&mut self) {
        self.engine.discovery_updated();
        let old: Vec<(String, Option<String>)> = self
            .catalog
            .providers
            .iter()
            .map(|p| (p.id.clone(), p.warning.clone()))
            .collect();
        let old_errors = std::mem::take(&mut self.catalog.errors);
        self.catalog = self.engine.catalog();
        for p in &self.catalog.providers {
            if let Some(w) = &p.warning
                && !old
                    .iter()
                    .any(|(id, ow)| *id == p.id && ow.as_ref() == Some(w))
            {
                self.transcript.notice(
                    Level::Warn,
                    format!("model discovery for {} failed: {w} · static models in use · /model shows details", p.label),
                );
            }
        }
        for e in self.catalog.errors.clone() {
            if !old_errors.contains(&e) {
                self.transcript
                    .notice(Level::Error, format!("config file skipped: {e}"));
            }
        }
        if let Overlay::Picker(p) = &mut self.overlay {
            p.discovery_updated(self.engine.discovering(), self.catalog.clone());
        }
    }

    /// Near the top of a transcript with contexts still on disk, read
    /// the next older one in the background; insert it when it arrives,
    /// keeping the rows on screen where they are.
    fn poll_older(&mut self) -> bool {
        if let Some((agent, n, rx)) = &self.older_job {
            let items = match rx.try_recv() {
                Ok(items) => items,
                Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                Err(_) => vec![],
            };
            let (agent, n) = (agent.clone(), *n);
            self.older_job = None;
            let mut sub = Transcript::default();
            fill(&mut sub, items, &self.tree, &agent);
            let on_screen = self.layout.locate(self.layout.top);
            let viewing = self.view.clone().unwrap_or_else(|| "root".into()) == agent;
            let Some(tr) = self.transcript_of(&agent) else {
                return true;
            };
            let before = tr.len();
            tr.insert_earlier(n, sub);
            let delta = tr.len() as isize - before as isize;
            if viewing
                && self.scroll.is_some()
                && let Some((e, row)) = on_screen
            {
                self.anchor_req = Some(((e as isize + delta).max(0) as usize, row, 0));
            }
            if viewing && self.search.is_some() {
                // The section went in at index 1, below the "on disk" row.
                self.search_loaded((delta.max(0) as usize).max(1));
            }
            return true;
        }
        let near_top = self.layout.top < self.layout.view_h.max(1) * 2;
        if near_top && let Some((agent, n)) = self.view_transcript().take_older() {
            let rx = self.engine.load_context(&agent, n);
            self.older_job = Some((agent, n, rx));
            return true;
        }
        false
    }

    /// Tell the engine whether the frontend is idle; it compacts root
    /// once per idle period when the model enables `autocompactAtIdle`.
    fn idle_tick(&mut self) {
        if self.read_only.is_some() || self.running || self.compacting {
            return;
        }
        let busy = !matches!(self.overlay, Overlay::None)
            || !self.prompts.is_empty()
            || self.last_key.elapsed() < Duration::from_secs(2);
        if self.engine.idle_tick(busy) {
            self.transcript.compacting();
            self.compacting = true;
            self.start();
            self.phase = Phase::Compacting;
        }
    }

    /// Replay startup typing after the first frame. A real Enter follows the
    /// normal submit path; bracketed-paste newlines remain composer text.
    /// Startup dialogs still require their usual explicit focus step.
    pub fn early_input(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        let mut pasted = false;
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => {
                    // CSI `ESC [ … final` and SS3 `ESC O x`.
                    match chars.next() {
                        Some('[') => {
                            let mut sequence = String::new();
                            while let Some(&n) = chars.peek() {
                                chars.next();
                                sequence.push(n);
                                if ('@'..='~').contains(&n) {
                                    break;
                                }
                            }
                            match sequence.as_str() {
                                "200~" => pasted = true,
                                "201~" => pasted = false,
                                "13u" | "13;1u" if !pasted => self.early_enter(&mut out),
                                "13;2u" | "27;2;13~" if !pasted => out.push('\n'),
                                _ => {}
                            }
                        }
                        Some('O') => {
                            chars.next();
                        }
                        Some('\r' | '\n') => out.push('\n'), // Alt+Enter
                        _ => {}
                    }
                }
                '\r' | '\n' => {
                    if c == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    if pasted {
                        out.push('\n');
                    } else {
                        self.early_enter(&mut out);
                    }
                }
                '\x7f' | '\x08' => {
                    out.pop();
                }
                '\t' => out.push(' '),
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
        if !out.is_empty() {
            self.composer.insert(&out);
            self.composer.edited();
            self.hint("typed while starting: kept in the composer");
        }
    }

    fn early_enter(&mut self, text: &mut String) {
        if !text.is_empty() {
            self.composer.insert(text);
            text.clear();
            self.composer.edited();
        }
        self.on_term(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    /// Finished background work (model discovery, usage reads); true when
    /// something changed.
    pub(super) fn poll_background(&mut self) -> bool {
        let mut changed = false;
        if let Some(result) = self.engine.poll_model_selection() {
            changed = true;
            match result {
                Ok(id) => self.model_selected(id),
                Err(error) => self.transcript.notice(Level::Error, error),
            }
        }
        if let Some(result) = self.engine.poll_discovery() {
            changed = true;
            match result {
                Ok(()) => {
                    let old = std::mem::take(&mut self.catalog.errors);
                    self.catalog = self.engine.catalog();
                    for e in self.catalog.errors.clone() {
                        if !old.contains(&e) {
                            self.transcript
                                .notice(Level::Error, format!("config file skipped: {e}"));
                        }
                    }
                }
                Err(e) => self
                    .transcript
                    .notice(Level::Error, format!("model discovery: {e}")),
            }
            if let Overlay::Picker(p) = &mut self.overlay {
                p.discovery_updated(self.engine.discovering(), self.catalog.clone());
            }
        }
        if self.poll_goal_action() {
            changed = true;
        }
        if self.poll_shells() {
            changed = true;
        }
        if self.poll_async_errors() {
            changed = true;
        }
        if self.poll_agent_done() {
            changed = true;
        }
        // Live `/agents` rows; the decision count and held root mail.
        if self.tick.is_multiple_of(10) {
            if matches!(self.overlay, Overlay::Agents(_)) {
                self.refresh_panels();
                changed = true;
            }
            let held = !self.running && self.engine.mail_held();
            if held != self.mail_held {
                self.mail_held = held;
                changed = true;
            }
        }
        if self.tick.is_multiple_of(10) {
            self.idle_tick();
        }
        if self.poll_older() {
            changed = true;
        }
        // The engine's usage cache is read without I/O; a second is fresh enough.
        if self.tick.is_multiple_of(10) || matches!(self.overlay, Overlay::Usage(_)) {
            let fresh = self.engine.provider_usage();
            if format!("{fresh:?}") != format!("{:?}", self.usage) {
                self.usage = fresh;
                changed = true;
            }
        }
        changed
    }

    /// Every enabled provider's usage: the background read merged with live
    /// quota reports and this session's spend for budgets.
    fn provider_usage(&self) -> Vec<crate::usage::ProviderUsage> {
        let mut out = vec![];
        for p in self.catalog.providers.iter().filter(|p| p.enabled) {
            let models: Vec<_> = self
                .catalog
                .models
                .iter()
                .filter(|m| m.provider == p.id)
                .collect();
            let mut u = self
                .usage
                .iter()
                .find(|u| u.provider == p.id)
                .cloned()
                .unwrap_or_else(|| crate::usage::ProviderUsage {
                    provider: p.id.clone(),
                    label: p.label.clone(),
                    ..Default::default()
                });
            u.label = p.label.clone();
            u.subscription = !models.is_empty() && models.iter().all(|m| m.subscription);
            if let Some(q) = self.quotas.get(&p.id) {
                match u.limits.iter_mut().find(|l| l.id == "quota") {
                    Some(l) => *l = q.clone(),
                    None => u.limits.insert(0, q.clone()),
                }
            }
            out.push(u);
        }
        out
    }

    /// The status-line quota badge: the active provider's tightest limit.
    fn quota_badge(&self) -> Option<(String, f64)> {
        let provider = self.active_info()?.provider.clone();
        let u = self
            .provider_usage()
            .into_iter()
            .find(|u| u.provider == provider)?;
        let l = u.tightest()?;
        // Quiet below half; /usage always shows everything.
        let pct = l.percent().filter(|p| *p >= 50.0)?;
        Some((crate::usage::badge(&l)?, pct))
    }

    fn usage_data(&self) -> UsageData {
        UsageData {
            providers: self.provider_usage(),
            refreshing: self.usage.iter().any(|u| u.refreshing),
            active: self.active_info().map(|m| m.provider.clone()),
        }
    }

    /// Both receivers at once, for the event loop's select.
    pub fn channels(&mut self) -> (&mut Option<EventRx>, &mut Option<crate::engine::TermRx>) {
        (&mut self.events, self.terms.rx_mut())
    }

    /// Handle one more queued engine event without waiting; false when
    /// none is queued (or the channel closed).
    pub fn next_engine_event(&mut self) -> bool {
        let Some(rx) = &mut self.events else {
            return false;
        };
        match rx.try_recv() {
            Ok(ev) => {
                self.on_engine(Some(ev));
                true
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => false,
            Err(_) => {
                self.on_engine(None);
                false
            }
        }
    }

    /// A `$EDITOR` run is pending (the event loop hands it the terminal).
    pub fn wants_editor(&self) -> bool {
        self.editor.is_some()
    }

    /// Run the pending `$EDITOR` request through `edit` (the text in, the
    /// edited text or None out) and apply the result.
    pub async fn run_editor<F>(&mut self, edit: impl FnOnce(String) -> F)
    where
        F: std::future::Future<Output = Result<Option<String>, String>>,
    {
        let Some(req) = self.editor.take() else {
            return;
        };
        let result = edit(req.text.clone()).await;
        self.editor_done(req, result);
    }

    /// The new mouse capture mode, once after each toggle.
    pub fn take_mouse_change(&mut self) -> Option<bool> {
        std::mem::take(&mut self.mouse_changed).then_some(self.mouse)
    }

    /// Draw a frame (and emit its links) when something changed; true
    /// when it drew.
    pub fn redraw(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> Result<bool, String> {
        if !self.dirty {
            return Ok(false);
        }
        self.dirty = false;
        terminal.draw(|f| self.draw(f)).map_err(|e| e.to_string())?;
        crate::links::emit(&self.links);
        Ok(true)
    }

    /// The event loop's 100 ms timer: advance the animation tick, redraw
    /// when something moves, and update the attention state.
    pub fn on_timer(&mut self) {
        self.tick += 1;
        if self.on_tick() || self.animating() || self.has_hint() {
            self.dirty = true;
        }
        self.attention_tick();
    }

    /// Whether something animates (a turn, a live subagent, a fading
    /// abandoned call, a drag auto-scroll).
    pub fn animating(&self) -> bool {
        self.running
            || self.tree.running() > 0
            || self.transcript.has_abandoned()
            || self.press.as_ref().is_some_and(|p| p.dragged)
    }

    pub async fn shutdown(&mut self) {
        self.engine.close().await;
    }

    /// The session to name on exit, when it has any messages.
    pub fn resume_hint(&self) -> Option<String> {
        self.transcript
            .has_conversation()
            .then(|| self.engine.session_id())
            .filter(|id| !id.is_empty())
    }

    pub fn has_hint(&self) -> bool {
        self.hint
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() < Duration::from_secs(4))
    }

    fn hint(&mut self, text: impl Into<String>) {
        self.hint = Some((text.into(), Instant::now()));
    }

    fn active_info(&self) -> Option<&crate::engine::ModelInfo> {
        self.engine
            .active_model()
            .and_then(|id| self.catalog.get(&id))
    }

    // ---- sessions ------------------------------------------------------

    /// Reset the UI for the engine's (new) session and replay its transcript.
    fn load_session(&mut self, events: EventRx, how: &str) {
        self.events = Some(events);
        self.queued_snapshots.clear();
        self.shells.clear();
        self.terms_reset();
        self.running = false;
        self.compacting = false;
        self.phase = Phase::Idle;
        self.queue_focus = None;
        self.queue_scroll = 0;
        self.queue_edit = None;
        self.stats.clear();
        self.prompts.clear();
        self.read_only = None;
        self.transcript.clear();
        self.scroll = None;
        self.warned = false;
        self.tree.clear();
        self.view = None;
        self.strip_focus = false;
        self.strip_scroll = 0;
        self.show_finished = false;
        self.sel = None;
        self.focus = None;
        self.pending_switch = None;
        self.mcp_check_until = Some(Instant::now() + Duration::from_secs(15));
        self.mcp_problems.clear();
        self.older_job = None;
        self.skills = self.engine.skills();
        for v in self.engine.agents() {
            self.tree.add(v, false);
        }
        for j in self.engine.jobs() {
            self.tree.upsert_job(j);
        }
        self.composer.reset_history(self.engine.prompt_history());
        let replay = self.engine.replay();
        for u in replay.usage {
            self.record_usage(u);
        }
        let earlier = replay.earlier.len() + replay.older.len();
        if !replay.older.is_empty() {
            self.transcript.push_older("root", replay.older);
        }
        for items in replay.earlier {
            let mut sub = Transcript::default();
            fill(&mut sub, items, &self.tree, "root");
            self.transcript.push_earlier(sub);
        }
        let resumed = !replay.current.is_empty() || earlier > 0;
        fill(&mut self.transcript, replay.current, &self.tree, "root");
        // Show the context meter at once, before the first send.
        let rc = self.engine.resume_context();
        self.ctx = (rc.used, rc.window);
        self.ctx_estimate = rc.estimate;
        self.ctx_unknown = rc.unknown;
        self.transcript.seal();
        self.deferred_goal = None;
        self.refresh_goal();
        self.goal_announced = self.goal.as_ref().map(|g| g.state_text());
        self.title = self.engine.title();
        for q in self.engine.pending_questions() {
            self.prompts.push_back(Prompt::Question(q, false));
        }
        // Goal dialogs waiting from before: draft, proposals, roadblock.
        for ev in self.engine.goal_requests() {
            if let Some(p) = goal_prompt(ev) {
                self.prompts.push_back(Prompt::Goal(p, false));
            }
        }
        for g in self.engine.pending_guidance() {
            self.prompts.push_front(Prompt::Guidance(g, false));
        }
        // Pending scope requests come back as rows and, for root's,
        // the owner dialog.
        for v in self.engine.pending_scopes() {
            self.on_scope(v);
        }
        let model = self
            .engine
            .active_model()
            .unwrap_or_else(|| "no model: set defaultModel or use /model".into());
        self.config_notices();
        self.transcript.notice(
            Level::Info,
            format!(
                "{how} {} · {model} · {} · {} · {} subagents · /help for keys",
                short(&self.engine.session_id()),
                self.engine.cwd().display(),
                if resumed {
                    format!("restored, total {}", self.engine.cost())
                } else {
                    "new".into()
                },
                if self.engine.subagents().0 {
                    "flat"
                } else {
                    "tree"
                }
            ),
        );
        self.decision_count = self.engine.decision_open_count();
    }

    /// Switch sessions; asks first while background work runs (it is stopped).
    /// Explain a missing model list and skipped provider/MCP files.
    fn config_notices(&mut self) {
        if self.catalog.models.is_empty() {
            let text = self.catalog.no_models();
            self.transcript.notice(Level::Error, text);
        }
        for e in self.catalog.errors.clone() {
            self.transcript
                .notice(Level::Error, format!("config file skipped: {e}"));
        }
    }

    fn switch(&mut self, target: EngineTarget, how: &str) -> bool {
        self.switch_refill(target, how, None)
    }

    fn switch_refill(&mut self, target: EngineTarget, how: &str, refill: Option<String>) -> bool {
        if self.running {
            self.hint("cancel the running turn first");
            return false;
        }
        let (jobs, children) = self.engine.running_work();
        let terms = self.terms.open_private();
        if jobs + children + terms > 0 {
            let mut body = vec![];
            if jobs + children > 0 {
                body.push(format!(
                    "{}. Leaving the session stops {}.",
                    work_text(jobs, children),
                    if jobs + children == 1 { "it" } else { "them" }
                ));
            }
            if terms > 0 {
                body.push(format!(
                    "{terms} private terminal{} open; leaving closes {} without sharing any output.",
                    if terms == 1 { " is" } else { "s are" },
                    if terms == 1 { "it" } else { "them" }
                ));
            }
            let dialog = Dialog::new(
                "Leave this session?",
                Body::Markdown(body.join("\n\n")),
                vec![
                    DialogOption::new("go", "Stop them and switch")
                        .key('y')
                        .guarded(),
                    DialogOption::new("stay", "Stay").key('n'),
                ],
            )
            .select("stay");
            self.overlay = Overlay::Dialog(
                Box::new(dialog),
                DialogPurpose::Switch(target, how.to_string(), refill),
            );
            return false;
        }
        self.switch_now(target, how, refill)
    }

    fn switch_now(&mut self, target: EngineTarget, how: &str, refill: Option<String>) -> bool {
        match self.engine.switch(target) {
            Ok(rx) => {
                self.load_session(rx, how);
                if let Some(text) = refill {
                    self.composer.set_wire(&text);
                    self.composer.edited();
                }
                true
            }
            Err(OpenError::Locked(info)) => {
                self.lock_dialog(info);
                false
            }
            Err(OpenError::Other(e)) => {
                self.transcript.notice(Level::Error, e);
                false
            }
        }
    }

    fn lock_dialog(&mut self, info: LockInfo) {
        let alive = match info.alive {
            Some(true) => "**alive** on this host — another cockpit-code is probably using it",
            Some(false) => "**not alive** on this host — likely a crash; safe to claim",
            None => "on another host — liveness unknown",
        };
        let title = if info.title.is_empty() {
            String::new()
        } else {
            format!(" “{}”", truncate(&info.title, 50))
        };
        let body = format!(
            "Session `{}`{title} is locked.\n\n- holder: pid **{}** on `{}`\n- since: {} ago\n- pid: {alive}",
            short(&info.session_id),
            info.pid,
            info.host,
            info.age
        );
        let dialog = Dialog::new(
            "Session is locked",
            Body::Markdown(body),
            vec![
                DialogOption::new("dont", "Don't resume")
                    .key('n')
                    .describe("stay in the current session"),
                DialogOption::new("fork", "Fork")
                    .key('f')
                    .describe("open a copy with no lock conflict"),
                {
                    let claim = DialogOption::new("claim", "Claim lock").key('c');
                    // Taking a live holder's lock makes it read-only.
                    if info.alive == Some(false) {
                        claim.describe("take over; the holder is gone")
                    } else {
                        claim
                            .guarded()
                            .describe("take over; the other process becomes read-only")
                    }
                },
            ],
        )
        // A dead holder on this host: claiming is the safe, likely choice.
        .select(if info.alive == Some(false) {
            "claim"
        } else {
            "dont"
        })
        // Shown at start-up, where keys typed without looking would fork
        // or claim: an explicit ↑/↓ (or a click) comes first.
        .require_focus();
        self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Lock(info));
    }

    fn open_resume(&mut self) {
        let (rows, problems) = match self.engine.list_sessions(self.list_all) {
            Ok(r) => r,
            Err(e) => return self.transcript.notice(Level::Error, e),
        };
        // Unreadable sessions are counted in the footer, not listed in the
        // transcript on every open (`cockpit-code sessions --all` names them).
        let skipped = problems.len();
        let rows = rows
            .into_iter()
            .map(|r| {
                let model = r
                    .model
                    .as_deref()
                    .map(|m| self.catalog.get(m).map_or(m, |i| i.label.as_str()))
                    .unwrap_or("default model")
                    .to_string();
                let mut detail = format!("{:>4} · {model} · {}", r.age, r.cost);
                if r.stale {
                    detail.push_str(" · stale lock");
                } else if r.locked {
                    detail.push_str(" · locked");
                }
                if self.list_all {
                    detail.push_str(&format!(" · {}", r.cwd));
                }
                list::Row {
                    id: r.id,
                    label: crate::paste::collapse(&r.title),
                    detail,
                    alert: false,
                }
            })
            .collect();
        let filter = self.resume_filter.clone();
        let picker = ListPicker::new(
            if self.list_all {
                "Resume · all sessions"
            } else {
                "Resume · this directory"
            },
            rows,
        )
        .with_filter(&filter)
        .with_footer(format!(
            "↑↓ select · enter resume · tab {} · ctrl+r rename · ctrl+d delete · esc close{}",
            if self.list_all {
                "this directory"
            } else {
                "all sessions"
            },
            match skipped {
                0 => String::new(),
                1 => " · 1 unreadable session skipped".into(),
                n => format!(" · {n} unreadable sessions skipped"),
            }
        ))
        .with_empty(if filter.is_empty() {
            "no other sessions"
        } else {
            "no sessions match"
        });
        self.overlay = Overlay::List(Box::new(picker), ListPurpose::Resume);
        self.resume_preview();
    }

    /// `/btw [text]`: the viewed agent's side chat, a hidden fork that
    /// never touches its context; `text` is sent at once.
    fn open_btw(&mut self, text: &str) {
        let agent = self.view.clone().unwrap_or_else(|| "root".into());
        let mut chat = ForkChat::side_chat(self.tree.label(&agent));
        let opened = self.engine.open_side_chat(&agent);
        let earlier = self.earlier_snapshots(&opened);
        chat.connect_with(opened, earlier, text, |fork| {
            self.engine.fork_chat_send(fork, text.to_string())
        });
        self.overlay = Overlay::ForkChat(Box::new(chat));
    }

    /// `/diff` on tab `tab` (0 worktree, 1 staged, 2 this turn).
    fn open_diff(&mut self, tab: usize) {
        let cwd = self.engine.cwd().to_path_buf();
        let loaded = match tab {
            0 => crate::diffview::worktree(&cwd),
            1 => crate::diffview::staged(&cwd),
            _ => {
                // The viewed agent's edits since its last user message.
                let edits = self.view_transcript().turn_edits();
                Ok(crate::diffview::turn(&edits, &cwd))
            }
        };
        self.overlay = Overlay::Diff(Box::new(crate::diffview::DiffView::new(tab, loaded)));
    }

    /// The selected session's last messages under the picker.
    fn resume_preview(&mut self) {
        let Overlay::List(l, ListPurpose::Resume) = &mut self.overlay else {
            return;
        };
        let engine = &self.engine;
        l.preview_selected(|id| {
            let t = theme();
            id.map(|id| engine.session_preview(&id, 3))
                .unwrap_or_default()
                .into_iter()
                .map(|(role, text)| {
                    // Plain text: no markdown marks, one line.
                    let text = crate::paste::collapse(&text)
                        .replace("```", " ")
                        .replace("**", "")
                        .replace('`', "")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let (mark, style) = if role == "user" {
                        ("▌ ", Style::default().fg(t.accent))
                    } else {
                        ("  ", Style::default().fg(t.fog))
                    };
                    Line::from(vec![
                        Span::styled(mark, style),
                        Span::styled(
                            truncate(&text, 400),
                            style.fg(if role == "user" { t.ink } else { t.fog }),
                        ),
                    ])
                })
                .collect()
        });
    }

    fn open_fork_at(&mut self) {
        let candidates = self.engine.fork_candidates();
        if candidates.is_empty() {
            return self.hint("no messages to fork at");
        }
        let rows = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| list::Row {
                id: i.to_string(),
                label: crate::paste::collapse(&c.text),
                detail: format!("context {} · line {}", c.context, c.line + 1),
                alert: false,
            })
            .collect();
        let picker = ListPicker::new("Fork before message (it returns to the composer)", rows)
            .select_last()
            .with_footer("↑↓ select · enter fork · esc close");
        let points = candidates
            .into_iter()
            .map(|c| (c.context, c.line, c.text))
            .collect();
        self.overlay = Overlay::List(Box::new(picker), ListPurpose::ForkAt(points));
    }

    // ---- prompts (approvals, questions) ---------------------------------

    fn pending(&self) -> usize {
        self.prompts.len()
    }

    /// Show the next waiting approval or question when nothing else is open.
    fn next_prompt(&mut self) {
        if !matches!(self.overlay, Overlay::None) {
            return;
        }
        // Never pop a dialog over a terminal being typed in: keys meant
        // for it (a password) must not land in a dialog field. The
        // prompt waits (status line) until ctrl+] leaves the terminal.
        if self.term_typing() {
            return;
        }
        if self.parked_fork.is_some()
            && !self.prompts.iter().any(|p| match p {
                Prompt::Approval(a) => fork_approval(a),
                Prompt::Question(q, false) => q.blocking,
                _ => false,
            })
        {
            self.overlay = Overlay::ForkChat(self.parked_fork.take().unwrap());
            return;
        }
        let Some(i) = self.prompts.iter().position(|p| {
            !matches!(
                p,
                Prompt::Question(_, true)
                    | Prompt::Goal(_, true)
                    | Prompt::Guidance(_, true)
                    | Prompt::Scope(_, true)
            )
        }) else {
            return;
        };
        self.prompt_title.clear();
        if let Prompt::Goal(g, _) = &self.prompts[i] {
            let g = g.clone();
            return self.goal_prompt_dialog(&g);
        }
        if let Prompt::Guidance(g, _) = &self.prompts[i] {
            let g = g.clone();
            return self.guidance_dialog(&g);
        }
        if let Prompt::Scope(v, _) = &self.prompts[i] {
            let v = v.clone();
            return self.scope_dialog(&v);
        }
        match &self.prompts[i] {
            Prompt::Approval(a) => {
                let crumb = self.tree.crumb(&a.breadcrumb);
                let mut md = format!("**{}** requested by **{crumb}**", a.tool);
                if let Some(job) = &a.job {
                    md.push_str(&format!(" · background job `{}`", short(job)));
                }
                md.push_str("\n\n");
                if let Some(target) = &a.target {
                    md.push_str(&format!("target: `{target}`\n\n"));
                }
                match &a.verdict {
                    Some((v, reason)) => md.push_str(&format!(
                        "safety model ({}): **{v}** — {reason}\n\n",
                        self.engine.mode().as_str()
                    )),
                    None => md.push_str(&format!("reason: {}\n\n", a.reason)),
                }
                if let Some(cmd) = &a.command {
                    // Web tools describe their request as a command too.
                    let lang = if a.tool == "bash" { "bash" } else { "text" };
                    md.push_str(&format!("```{lang}\n{}\n```\n", cmd.trim_end()));
                }
                if let Some(diff) = &a.diff
                    && !diff.trim().is_empty()
                {
                    md.push_str(&format!("```diff\n{}\n```\n", diff.trim_end()));
                }
                let session = match &a.grant_key {
                    Some(k) => format!("until this session ends: {}", grant_label(k)),
                    None => "not grantable for compound/wrapped calls; counts as once".into(),
                };
                let title = if a.agent == "root" {
                    format!("Allow {}?", a.tool)
                } else {
                    format!("Allow {}? · {crumb}", a.tool)
                };
                let mut options = vec![
                    DialogOption::new("once", "Allow once").key('o').guarded(),
                    DialogOption::new("session", "Allow for session")
                        .key('s')
                        .guarded()
                        .describe(session),
                    DialogOption::new("deny", "Deny with feedback")
                        .key('d')
                        .describe("the feedback is returned to the model"),
                ];
                if a.agent != "root" && self.view.as_deref() != Some(a.agent.as_str()) {
                    options.push(
                        DialogOption::new("jump", "Jump to agent")
                            .key('j')
                            .describe("open its transcript; this dialog stays open"),
                    );
                }
                let dialog = Dialog::new(title, Body::Markdown(md), options)
                    .with_text("feedback for the model")
                    .text_option("deny", false);
                let id = a.id.clone();
                self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Approval(id));
            }
            Prompt::Goal(..) | Prompt::Guidance(..) | Prompt::Scope(..) => {}
            Prompt::Question(q, _) => {
                let first = q.clone();
                // Several open questions: one dialog over all of them, grouped
                // by asker; any subset can be answered at once.
                let mut open: Vec<QuestionView> = self
                    .prompts
                    .iter()
                    .filter_map(|p| match p {
                        Prompt::Question(q, false) => Some(q.clone()),
                        _ => None,
                    })
                    .collect();
                if open.len() > 1 {
                    // Group by asker, root first, keeping arrival order.
                    let mut order: Vec<String> = vec![];
                    for q in &open {
                        if !order.contains(&q.asker) {
                            order.push(q.asker.clone());
                        }
                    }
                    order.sort_by_key(|a| a != "root");
                    open.sort_by_key(|q| order.iter().position(|a| *a == q.asker));
                } else {
                    open = vec![first.clone()];
                }
                let calls: Vec<(questions::Call, Vec<crate::engine::QItem>)> = open
                    .iter()
                    .map(|q| (self.question_call(q), q.questions.clone()))
                    .collect();
                let title = if calls.len() > 1 {
                    format!("{} pending questions", calls.len())
                } else if first.blocking {
                    format!(
                        "{} is waiting for your answer",
                        self.tree.label(&first.asker)
                    )
                } else {
                    format!("Question from {}", self.tree.label(&first.asker))
                };
                self.overlay = Overlay::Questions(Box::new(QDialog::new(title, calls)));
            }
        }
    }

    fn question_call(&self, q: &QuestionView) -> questions::Call {
        let mut md = String::new();
        if q.asker != "root" {
            md.push_str(&format!("asked by **{}**", self.tree.crumb(&q.breadcrumb)));
            if q.blocking {
                md.push_str(" · _waiting for you_");
            }
            if let Some(note) = q.note.as_deref().filter(|n| !n.trim().is_empty()) {
                md.push_str(&format!(" · escalated with note: _{}_", note.trim()));
            }
        }
        questions::Call {
            id: q.id.clone(),
            asker: q.asker.clone(),
            label: self.tree.label(&q.asker),
            preamble: md,
            can_jump: q.asker != "root" && self.view.as_deref() != Some(q.asker.as_str()),
        }
    }

    fn remove_prompt(&mut self, id: &str) {
        self.prompts.retain(|p| match p {
            Prompt::Approval(a) => a.id != id,
            Prompt::Question(q, _) => q.id != id,
            Prompt::Goal(g, _) => g.id() != id,
            Prompt::Guidance(g, _) => g.id != id,
            Prompt::Scope(v, _) => v.id != id,
        });
    }

    /// Close an approval dialog whose request is no longer pending.
    fn drop_stale_dialog(&mut self) {
        if let Overlay::Dialog(_, DialogPurpose::Approval(id)) = &self.overlay
            && !self
                .prompts
                .iter()
                .any(|p| matches!(p, Prompt::Approval(a) if a.id == *id))
        {
            self.overlay = Overlay::None;
        }
    }

    fn approve(&mut self, id: &str, choice: ApprovalChoice) {
        let label = match &choice {
            ApprovalChoice::Once => "allowed once".to_string(),
            ApprovalChoice::Session => "allowed for session".to_string(),
            ApprovalChoice::Deny(f) => format!("denied: {f}"),
        };
        let (tool, agent) = self
            .prompts
            .iter()
            .find_map(|p| match p {
                Prompt::Approval(a) if a.id == id => Some((a.tool.clone(), a.agent.clone())),
                _ => None,
            })
            .unzip();
        self.remove_prompt(id);
        // Its row's timer resumes, unless another approval of the agent waits.
        if let Some(agent) = agent
            && !self
                .prompts
                .iter()
                .any(|p| matches!(p, Prompt::Approval(a) if a.agent == agent))
            && let Some(tr) = self.transcript_of(&agent)
        {
            tr.approval_done();
        }
        match self.engine.respond(id, choice) {
            Ok(()) => self
                .transcript
                .notice(Level::Info, format!("{} {label}", tool.unwrap_or_default())),
            Err(e) => self.transcript.notice(Level::Error, e),
        }
    }

    // ---- engine events -------------------------------------------------

    pub fn on_engine(&mut self, event: Option<UiEvent>) {
        self.dirty = true;
        let ids: Vec<String> = self.prompts.iter().map(prompt_key).collect();
        self.on_engine_event(event);
        // A prompt that just started waiting wants the user.
        if let Some(p) = self.prompts.iter().find(|p| !ids.contains(&prompt_key(p))) {
            let reason = Self::prompt_reason(p);
            self.attend(reason);
        }
    }

    fn on_engine_event(&mut self, event: Option<UiEvent>) {
        let Some(event) = event else {
            self.events = None;
            if self.running {
                self.finish_turn("error".into());
            }
            return;
        };
        let busy = |app: &mut Self, phase| {
            if app.phase != Phase::Cancelling && !app.compacting {
                app.phase = phase;
            }
        };
        match event {
            UiEvent::Text(d) => {
                if self.compacting {
                    self.transcript.append_compacting(&d);
                } else {
                    busy(self, Phase::Writing);
                    self.transcript.append_text(&d);
                }
            }
            UiEvent::Reasoning(d) => {
                if !self.compacting {
                    busy(self, Phase::Thinking);
                    self.transcript.append_reasoning(&d);
                }
            }
            UiEvent::ReplyEnd => self.transcript.end_reply(),
            UiEvent::ToolDelta {
                index,
                id,
                name,
                args,
            } => {
                if !self.compacting {
                    busy(self, Phase::Writing);
                    self.transcript.tool_delta(index, id, name, &args);
                }
            }
            UiEvent::ToolStart { id, name, args } => {
                busy(self, Phase::Tool);
                self.transcript.tool_start(id, name, args);
                link_tasks(&mut self.transcript, &self.tree);
            }
            UiEvent::ScriptCall {
                job,
                parent,
                call,
                result,
                status,
            } => self
                .transcript
                .script_call(job.as_deref(), parent.as_deref(), call, result, &status),
            UiEvent::SafetyReview {
                call_id,
                model,
                result,
            } => {
                let label = self.model_label(Some(&model));
                self.transcript.review(&call_id, label, result);
            }
            UiEvent::ToolEnd {
                id,
                output,
                is_error,
                details,
            } => {
                self.transcript.tool_end(&id, output, is_error, details);
                link_tasks(&mut self.transcript, &self.tree);
                self.git.poke();
            }
            UiEvent::MovedToBackground {
                call_id,
                job,
                reason,
            } => {
                self.transcript.moved_to_background(&call_id, &job, &reason);
            }
            UiEvent::Interrupted => self.transcript.interrupted(),
            UiEvent::QueueChanged => self.queue_changed(),
            UiEvent::UserRecorded(text) => self.composer.remember(text),
            UiEvent::Usage(u) => self.record_usage(u),
            UiEvent::Retry {
                attempt,
                delay_ms,
                reason,
            } => {
                busy(self, Phase::Retrying);
                self.transcript
                    .retry_notice(&retry_text(attempt, delay_ms, reason.as_deref()));
            }
            UiEvent::StreamReset => self.transcript.stream_reset(),
            UiEvent::Reconnecting { attempt, delay_ms } => {
                busy(self, Phase::Reconnecting);
                self.transcript
                    .retry_notice(&reconnect_text(attempt, delay_ms));
            }
            UiEvent::ModelSwitched { from, to, reason } => {
                let (f, t) = (self.model_label(Some(&from)), self.model_label(Some(&to)));
                self.backup_model = Some(t.clone());
                self.transcript
                    .notice(Level::Warn, switch_text(&f, &t, &reason));
            }
            UiEvent::Error { kind, message } => {
                if (self.user_cancelled && message.contains("cancelled"))
                    || (kind == "read-only" && self.read_only.is_some())
                {
                    return;
                }
                let text = match kind {
                    "error" => message,
                    _ => format!("{kind}: {message}"),
                };
                self.transcript.notice(Level::Error, text);
                if kind == "context" {
                    self.transcript.notice(
                        Level::Info,
                        "/compact [focus] summarizes this context · /fork branches from an earlier message · /new starts fresh · /model picks a larger window",
                    );
                }
            }
            UiEvent::Context {
                used,
                window,
                reserved,
                warning,
                known,
                estimate,
            } => {
                self.ctx_estimate = estimate;
                self.ctx_unknown = !known;
                // Never imply a limit the provider did not report.
                self.ctx = if known {
                    (used, window.or(self.ctx.1))
                } else {
                    (0, None)
                };
                // The engine warns at 90% including the output reservation.
                let pct = window.map(|w| (used + reserved) as f64 / w.max(1) as f64 * 100.0);
                // At or past the limit the typed error follows instead.
                let over = pct.is_some_and(|p| p >= 100.0);
                if (warning || pct.is_some_and(|p| p >= 90.0)) && !over && !self.warned {
                    self.warned = true;
                    self.transcript.notice(
                        Level::Warn,
                        format!(
                            "context {:.0}% full ({} used + {} reserved for output of {}) · /compact [focus] to summarize, or /fork to branch",
                            pct.unwrap_or(90.0),
                            fmt_tokens(used),
                            fmt_tokens(reserved),
                            window.map_or("?".into(), fmt_tokens)
                        ),
                    );
                } else if pct.is_some_and(|p| p < 80.0) {
                    self.warned = false;
                }
            }
            UiEvent::Approval(a) => {
                if let Some(tr) = self.transcript_of(&a.agent) {
                    tr.approval_wait(&a.call_id, a.parent_call_id.as_deref());
                }
                if fork_approval(&a) && matches!(self.overlay, Overlay::ForkChat(_)) {
                    // The fork chat is waiting on this approval: show it now.
                    if let Overlay::ForkChat(c) =
                        std::mem::replace(&mut self.overlay, Overlay::None)
                    {
                        self.parked_fork = Some(c);
                    }
                    self.prompts.push_front(Prompt::Approval(a));
                } else {
                    self.prompts.push_back(Prompt::Approval(a));
                }
            }
            UiEvent::Question(q) => {
                let known = self
                    .prompts
                    .iter()
                    .any(|p| matches!(p, Prompt::Question(x, _) if x.id == q.id));
                if !known {
                    if q.blocking {
                        // A blocked asker opens its dialog at once.
                        if !matches!(
                            &self.overlay,
                            Overlay::Dialog(_, DialogPurpose::Approval(_)) | Overlay::Questions(_)
                        ) && let Overlay::ForkChat(c) =
                            std::mem::replace(&mut self.overlay, Overlay::None)
                        {
                            self.parked_fork = Some(c);
                        }
                        self.prompts.push_front(Prompt::Question(q, false));
                    } else {
                        self.prompts.push_back(Prompt::Question(q, false));
                    }
                }
            }
            UiEvent::ForkChat {
                fork,
                kind,
                text,
                messages,
                call,
                result,
            } => self.fork_chat_event(fork, kind, text, messages, call, result),
            UiEvent::ForkAnswered { id, asker, summary } => {
                self.remove_prompt(&id);
                if matches!(&self.overlay, Overlay::Questions(d) if d.has_call(&id)) {
                    self.overlay = Overlay::None;
                }
                let row = Kind::ForkAnswer {
                    question: id,
                    summary,
                };
                match self.transcript_of(&asker) {
                    Some(tr) if asker != "root" => tr.push(row),
                    _ => self.transcript.push(row),
                }
            }
            UiEvent::Answered(id) => {
                if !self.running {
                    self.wake_reasons.push("your answer".into());
                }
                self.remove_prompt(&id);
                if matches!(&self.overlay, Overlay::Questions(d) if d.has_call(&id)) {
                    self.overlay = Overlay::None;
                }
            }
            UiEvent::Scope(v) => {
                if !self.running && v.owner == "root" && v.status == "pending" && v.escalation.is_none() {
                    self.wake_reasons
                        .push(format!("scope request from {}", self.tree.label(&v.requester)));
                }
                self.on_scope(v)
            }
            UiEvent::Subscription {
                agent,
                changes,
                overflow,
            } => self.on_subscription(&agent, changes, overflow),
            UiEvent::Guidance(sel) => self.on_guidance(sel),
            UiEvent::Pruned(r) => {
                self.hint = None;
                let card = engine_o::prune_card(&r);
                self.transcript.card(card);
                // The context shrank; the next usage report has the number.
                if r.saved > 0 {
                    self.ctx.0 = r.after;
                }
            }
            UiEvent::GuidanceTrust(g) => {
                self.prompts.retain(|p| !matches!(p, Prompt::Guidance(x, _) if x.id == g.id));
                self.prompts.push_front(Prompt::Guidance(g, false));
            }
            UiEvent::RootCompactionSuggested { used, window } => {
                let pct = used.saturating_mul(100) / window.max(1);
                self.transcript
                    .card(tree_rows::compact_card(pct, used, window));
            }
            UiEvent::Tree { agent, row } => self.on_tree(agent, row),
            UiEvent::Terminal(info) => self.on_terminal(info),
            UiEvent::Decision { open_count } => {
                self.decision_count = open_count;
                self.refresh_panels();
            }
            UiEvent::IdleCompacted => self.transcript.notice(
                Level::Info,
                "compacted while idle, while the provider cache was warm · the full history stays on disk",
            ),
            UiEvent::DiscoveryUpdated => self.discovery_updated(),
            UiEvent::ContractChanged(agent) => {
                if agent == "root" {
                    self.refresh_goal();
                } else if let Overlay::Criteria(v) = &mut self.overlay
                    && v.agent == agent
                {
                    v.set_contract(self.engine.contract(&agent));
                }
            }
            UiEvent::GoalUpdate => {
                // Events can trail the state (one per save); announce the
                // engine's current state once per change.
                self.refresh_goal();
                self.announce_goal();
            }
            ev @ (UiEvent::GoalDraft(_)
            | UiEvent::ProposalRequest { .. }
            | UiEvent::RoadblockRequest { .. }) => {
                if let Some(p) = goal_prompt(ev) {
                    let id = p.id();
                    self.prompts
                        .retain(|q| !matches!(q, Prompt::Goal(g, _) if g.id() == id));
                    // A roadblock or draft is root's whole goal: show it first.
                    if matches!(p, GoalPrompt::Proposal { .. }) {
                        self.prompts.push_back(Prompt::Goal(p, false));
                    } else {
                        self.prompts.push_front(Prompt::Goal(p, false));
                    }
                }
            }
            UiEvent::Compacted(summary) => {
                // The summary is committed to the old context, then repeated as
                // the head of the new one; show it once.
                if self.compacting && self.transcript.compacted(summary) {
                    self.warned = false;
                    self.scroll = None;
                }
            }
            UiEvent::Claimed(message) => {
                self.read_only = Some(message.clone());
                self.engine.drain_queue("root");
                self.queue_edit = None;
                self.queue_changed();
                self.transcript.notice(
                    Level::Error,
                    format!(
                        "{message}: this view is now read-only · /fork copies the session, /new or /resume moves on"
                    ),
                );
            }
            UiEvent::Partial { status, text } => {
                self.transcript.partial(partial_text(&status, &text));
            }
            UiEvent::SettingsChanged => {}
            UiEvent::TitleChanged { title, auto } => {
                self.set_title(Some(title.clone()));
                if auto {
                    self.hint(format!("session titled: {title}"));
                }
            }
            UiEvent::HiddenFork { purpose, ok } => {
                if !ok && purpose == "auto_title" {
                    self.hint("automatic title failed; /rename sets one");
                }
            }
            UiEvent::TurnEnd { stop_reason } => self.finish_turn(stop_reason),
            UiEvent::AgentStarted(v) => {
                let (id, parent, task) = (v.id.clone(), v.parent.clone(), v.task.clone());
                let known = self.tree.index(&id).is_some();
                let i = self.tree.add(v, true);
                let a = &mut self.tree.agents[i];
                a.transcript.adopt_reasoning(&self.transcript);
                // A follow-up appends its new task to the existing transcript.
                if a.loaded || !known {
                    a.transcript.push(Kind::User(task));
                }
                let call = self.tree.get(&id).and_then(|a| a.view.call_id.clone());
                if let Some(tr) = self.transcript_of(&parent)
                    && let Some((row, follow_up)) = tr.task_row_for(&id, call.as_deref())
                    && let Some(link) = self.tree.link(&id, follow_up)
                    && let Some(tr) = self.transcript_of(&parent)
                {
                    tr.link_task(row, link);
                }
            }
            UiEvent::AgentStatus { id, status } => self.agent_status(&id, &status),
            UiEvent::AgentFinished { id, status } => {
                self.agent_status(&id, &status);
                if let Some(a) = self.tree.get_mut(&id) {
                    a.transcript.end_turn();
                    a.transcript.notice(
                        if status == "done" {
                            Level::Info
                        } else {
                            Level::Error
                        },
                        format!("{} {status}", a.label),
                    );
                }
                // Its approvals ended with it.
                self.prompts
                    .retain(|p| !matches!(p, Prompt::Approval(a) if a.agent == id));
                self.drop_stale_dialog();
            }
            UiEvent::Child { id, event } => self.child_event(&id, *event),
            UiEvent::Message { from, to, text } => {
                if !self.running && to == "root" && !matches!(from.as_str(), "user" | "jobs") {
                    self.wake_reasons
                        .push(format!("message from {}", self.tree.label(&from)));
                }
                self.tree_message(from, to, text)
            }
            UiEvent::Job(job) => {
                let terminal = !job.running();
                if terminal && !self.running && job.owner == "root" {
                    let who = job
                        .agent
                        .as_deref()
                        .map(|a| self.tree.label(a))
                        .unwrap_or_else(|| format!("{} job", job.kind));
                    self.wake_reasons.push(format!("{who} {}", job.status));
                }
                // A foreground child moved to the background gets
                // its badge from the running task job; not a second start.
                if !terminal
                    && job.kind == "task"
                    && let Some(child) = job.agent.as_deref()
                    && let Some(a) = self.tree.get_mut(child)
                {
                    a.view.background = true;
                }
                self.tree.upsert_job(job.clone());
                if terminal {
                    let what = match &job.agent {
                        Some(a) => format!("{} (background task)", self.tree.label(a)),
                        None => format!(
                            "{} `{}`",
                            job.kind,
                            truncate(job.command.lines().next().unwrap_or_default(), 60)
                        ),
                    };
                    let report = job
                        .report
                        .as_deref()
                        .and_then(|r| r.lines().find(|l| !l.trim().is_empty()))
                        .map(|l| format!(" · {}", l.trim()))
                        .unwrap_or_default();
                    if let Some(tr) = self.transcript_of(&job.owner) {
                        tr.notice(Level::Job, format!("job {what} {}{report}", job.status));
                    }
                }
                if matches!(self.overlay, Overlay::List(_, ListPurpose::Jobs)) {
                    let sel = match &self.overlay {
                        Overlay::List(l, _) => l.selected_id(),
                        _ => None,
                    };
                    self.open_jobs(sel.as_deref());
                }
            }
        }
        self.next_prompt();
    }

    fn transcript_of(&mut self, agent: &str) -> Option<&mut Transcript> {
        if agent == "root" {
            return Some(&mut self.transcript);
        }
        self.tree.get_mut(agent).map(|a| &mut a.transcript)
    }

    fn view_index(&self) -> Option<usize> {
        self.view.as_deref().and_then(|id| self.tree.index(id))
    }

    fn view_transcript(&mut self) -> &mut Transcript {
        match self.view_index() {
            Some(i) => &mut self.tree.agents[i].transcript,
            None => &mut self.transcript,
        }
    }

    fn agent_status(&mut self, id: &str, status: &str) {
        if id == "root" {
            // A turn the engine started by itself: queued next-turn input
            // after a turn ended or a question answer.
            let starting = status == "running" || status.starts_with("queued");
            if starting && !self.running && !self.compacting && self.read_only.is_none() {
                self.wake_header();
                self.transcript.begin_attempt();
                self.start();
                self.phase = Phase::Waiting;
            }
            return;
        }
        if let Some(a) = self.tree.get_mut(id) {
            a.view.status = status.to_string();
            if status == "compacting" && !a.compacting {
                a.compacting = true;
                a.transcript.compacting();
            } else if status != "compacting" && a.compacting {
                a.compacting = false;
                a.transcript.end_compacting();
            }
        }
        self.transcript.child_status(id, status);
        for a in &mut self.tree.agents {
            a.transcript.child_status(id, status);
        }
    }

    /// A subagent's display event, rendered into its own transcript.
    fn child_event(&mut self, id: &str, event: UiEvent) {
        // A child's live quota report is the provider's too.
        if let UiEvent::Usage(u) = &event
            && let Some(q) = &u.quota
        {
            self.quotas.insert(u.provider.clone(), q.clone());
        }
        let Some(i) = self.tree.index(id) else {
            return;
        };
        let a = &mut self.tree.agents[i];
        let tr = &mut a.transcript;
        let mut linked = false;
        match event {
            UiEvent::Text(d) => {
                if a.compacting {
                    tr.append_compacting(&d);
                } else {
                    tr.append_text(&d);
                }
            }
            UiEvent::Reasoning(d) if !a.compacting => tr.append_reasoning(&d),
            UiEvent::ToolDelta {
                index,
                id,
                name,
                args,
            } if !a.compacting => tr.tool_delta(index, id, name, &args),
            UiEvent::ToolStart { id, name, args } => {
                linked = name == "task";
                tr.tool_start(id, name, args);
            }
            UiEvent::ScriptCall {
                job,
                parent,
                call,
                result,
                status,
            } => tr.script_call(job.as_deref(), parent.as_deref(), call, result, &status),
            UiEvent::SafetyReview {
                call_id,
                model,
                result,
            } => {
                let label = self
                    .catalog
                    .get(&model)
                    .map_or(model.clone(), |m| m.label.clone());
                tr.review(&call_id, label, result);
            }
            UiEvent::ToolEnd {
                id,
                output,
                is_error,
                details,
            } => {
                tr.tool_end(&id, output, is_error, details);
                linked = true;
            }
            UiEvent::MovedToBackground {
                call_id,
                job,
                reason,
            } => tr.moved_to_background(&call_id, &job, &reason),
            UiEvent::Interrupted => tr.interrupted(),
            UiEvent::Retry {
                attempt,
                delay_ms,
                reason,
            } => tr.retry_notice(&retry_text(attempt, delay_ms, reason.as_deref())),
            UiEvent::StreamReset => tr.stream_reset(),
            UiEvent::Reconnecting { attempt, delay_ms } => {
                tr.retry_notice(&reconnect_text(attempt, delay_ms))
            }
            UiEvent::ModelSwitched { from, to, reason } => {
                tr.notice(Level::Warn, switch_text(&from, &to, &reason))
            }
            UiEvent::Error { kind, message } => tr.notice(
                Level::Error,
                match kind {
                    "error" => message,
                    _ => format!("{kind}: {message}"),
                },
            ),
            UiEvent::Context {
                used,
                window,
                known,
                estimate,
                ..
            } => {
                a.ctx_estimate = estimate;
                a.ctx_unknown = !known;
                a.ctx = if known {
                    (used, window.or(a.ctx.1))
                } else {
                    (0, None)
                }
            }
            UiEvent::TurnEnd { .. } => tr.seal(),
            UiEvent::ReplyEnd => tr.end_reply(),
            UiEvent::Partial { status, text } => tr.partial(partial_text(&status, &text)),
            // The summary closes the old context and heads the new one; show it once.
            UiEvent::Compacted(summary) => {
                if tr.compacted(summary) && a.compacting {
                    tr.compacting();
                }
            }
            _ => {}
        }
        if linked {
            let Self { tree, .. } = self;
            let mut tr = std::mem::take(&mut tree.agents[i].transcript);
            link_tasks(&mut tr, tree);
            tree.agents[i].transcript = tr;
        }
    }

    /// A tree message: steering shows as a user message in the child; child
    /// messages show as notices in the recipient's transcript.
    fn tree_message(&mut self, from: String, to: String, text: String) {
        if from == "jobs" {
            return; // job completions render from the Job event
        }
        if from == "user" {
            let text = skill_invocation(&text).unwrap_or(text);
            if let Some(tr) = self.transcript_of(&to) {
                tr.push(Kind::User(text));
                tr.begin_attempt();
            }
            return;
        }
        let body = message_text(&self.tree, &from, &to, &text);
        if let Some(tr) = self.transcript_of(&to) {
            tr.notice(Level::Message, body);
        }
    }

    fn record_usage(&mut self, u: Usage) {
        if let Some(q) = u.quota.clone() {
            self.quotas.insert(u.provider.clone(), q);
        }
        let s = self.stats.entry(u.model.clone()).or_default();
        s.calls += 1;
        s.input += u.input;
        s.output += u.output;
        s.cache_read += u.cache_read;
        s.cache_write += u.cache_write;
        match u.cost {
            Some(c) => {
                s.known_cost += c;
                s.known_calls += 1;
            }
            None => s.unknown_calls += 1,
        }
    }

    fn finish_turn(&mut self, stop_reason: String) {
        self.attend("turn finished");
        self.backup_model = None;
        self.git.poke();
        self.running = false;
        self.phase = Phase::Idle;
        self.transcript.end_turn();
        if let Some(id) = self.pending_switch.take() {
            if stop_reason == "compacted" && !self.user_cancelled {
                self.apply_model(id);
            } else {
                self.transcript.notice(
                    Level::Warn,
                    format!("compaction ended ({stop_reason}); the model was not switched"),
                );
            }
        }
        // Root approvals die with its turn; questions and live children's approvals outlive it.
        self.prompts.retain(|p| match p {
            Prompt::Question(..) | Prompt::Goal(..) | Prompt::Guidance(..) | Prompt::Scope(..) => {
                true
            }
            Prompt::Approval(a) => a.agent != "root",
        });
        self.drop_stale_dialog();
        if self.compacting {
            self.compacting = false;
            self.transcript.end_compacting();
            if stop_reason != "compacted" && !self.user_cancelled {
                self.transcript
                    .notice(Level::Error, format!("compaction ended: {stop_reason}"));
            }
        }
        if stop_reason == "cancelled" || self.user_cancelled {
            self.transcript.notice(Level::Info, "cancelled");
        }
        if let Some(e) = self.engine.reload() {
            self.transcript.notice(Level::Error, format!("config: {e}"));
        }
        let errors = std::mem::take(&mut self.catalog.errors);
        self.catalog = self.engine.catalog();
        if self.catalog.errors != errors {
            for e in self.catalog.errors.clone() {
                self.transcript
                    .notice(Level::Error, format!("config file skipped: {e}"));
            }
        }
        self.refresh_goal();
        self.announce_goal();
        self.skills = self.engine.skills();
        if self.user_cancelled {
            // A cancel stops the queue too; queued text returns to the
            // composer (an interrupt is not a cancel: it keeps the turn going).
            // A message being edited is already there and stays a plain draft.
            self.queue_edit = None;
            let queued = self.engine.drain_queue("root");
            if !queued.is_empty() {
                let mut text = queued
                    .into_iter()
                    .map(|q| q.text)
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !self.composer.is_empty() {
                    text = format!("{text}\n\n{}", self.composer.wire());
                }
                self.composer.set_wire(&text);
                self.composer.edited();
                self.hint("queued messages returned to the composer");
            }
            self.queue_changed();
        }
        // Held next-turn input starts the next turn in the engine; its root
        // `running` status starts it here (`agent_status`).
    }

    fn start(&mut self) {
        self.running = true;
        self.user_cancelled = false;
        self.turn_started = Instant::now();
        self.scroll = None;
    }

    fn send(&mut self, text: String) {
        // Mail rides along with the user's message.
        self.wake_reasons.clear();
        self.mail_held = false;
        self.transcript.push(Kind::User(text.clone()));
        self.transcript.begin_attempt();
        self.engine.send(text);
        self.start();
        self.phase = Phase::Waiting;
    }

    fn compact(&mut self, focus: Option<String>) {
        if self.read_only.is_some() {
            return self.hint("read-only session");
        }
        if self.running {
            return self.hint("wait for the turn to end, or esc to cancel it");
        }
        self.transcript.compacting();
        self.compacting = true;
        self.engine.compact(focus);
        self.start();
        self.phase = Phase::Compacting;
    }

    fn cancel(&mut self) {
        if self.running && !self.user_cancelled {
            self.user_cancelled = true;
            self.phase = Phase::Cancelling;
            self.engine.cancel();
        }
    }

    fn toggle_mouse(&mut self) {
        self.mouse = !self.mouse;
        self.mouse_changed = true;
        self.hint(if self.mouse {
            "mouse capture on: wheel scrolls, click expands"
        } else {
            "mouse capture off: native text selection"
        });
    }

    fn cycle_mode(&mut self) {
        let mut next = self.engine.mode().next();
        // Without a safety model, /exclusive allows manual and yolo only.
        if self.engine.exclusive().is_some_and(|e| e.safety.is_none()) {
            while matches!(next, Mode::Auto | Mode::Unattended) {
                next = next.next();
            }
        }
        // A session that froze the private terminal tool cannot become
        // unattended: the cycle passes over it, with a hint.
        if next == Mode::Unattended
            && let Err(e) = self.engine.set_mode(next)
        {
            next = next.next();
            self.set_mode(next);
            return self.hint(format!("{e} · skipped to {}", next.as_str()));
        }
        self.set_mode(next);
    }

    // ---- /exclusive ------------------------------------------------

    fn exclusive_command(&mut self, arg: &str) {
        if arg == "off" {
            return match self.engine.set_exclusive(None) {
                Ok(()) => self.hint("exclusive off: /model and subagent models are free again"),
                Err(e) => self.transcript.notice(Level::Error, e),
            };
        }
        if self.running {
            return self.hint("wait for the turn to end, or esc to cancel it");
        }
        self.catalog = self.engine.catalog();
        if !arg.is_empty() {
            let wanted = arg.to_lowercase();
            let found = self
                .catalog
                .models
                .iter()
                .find(|m| m.id.to_lowercase() == wanted || m.label.to_lowercase() == wanted)
                .or_else(|| {
                    self.catalog
                        .models
                        .iter()
                        .find(|m| m.id.to_lowercase().contains(&wanted))
                })
                .map(|m| m.id.clone());
            return match found {
                Some(id) => self.exclusive_ask_safety(id),
                None => self.hint(format!("no model matches {arg}")),
            };
        }
        self.exclusive_picker(None);
    }

    /// The model picker for `/exclusive`: the model, or its safety model.
    fn exclusive_picker(&mut self, model: Option<String>) {
        let picker = Picker::new(
            self.catalog.clone(),
            self.engine.active_model(),
            Default::default(),
            Default::default(),
            self.engine.safety_model(),
            picker::Purpose::Exclusive(match &model {
                None => "Exclusive model · every model use in this session".into(),
                Some(m) => format!("Safety model for exclusive {}", self.model_label(Some(m))),
            }),
            Default::default(),
        );
        self.exclusive_pick = Some(model);
        self.overlay = Overlay::Picker(Box::new(picker));
    }

    fn exclusive_ask_safety(&mut self, model: String) {
        let label = self.model_label(Some(&model));
        let dialog = Dialog::new(
            format!("Exclusive · {label}"),
            Body::Markdown(format!(
                "Every model use in this session — root, subagents, titles, variations, thinking summaries and `models` — goes to **{label}**. `/model` is locked until `/exclusive off`.\n\nUse a safety model?"
            )),
            vec![
                DialogOption::new("same", format!("Yes, {label} itself"))
                    .key('y')
                    .describe("reviews tool calls in auto and unattended"),
                DialogOption::new("pick", "Yes, pick another model…").key('p'),
                DialogOption::new("none", "No safety model")
                    .key('n')
                    .describe("auto and unattended need one: manual and yolo only"),
            ],
        );
        self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::ExclusiveSafety(model));
    }

    fn exclusive_ask_block(&mut self, model: String, safety: Option<String>) {
        let dialog = Dialog::new(
            "Exclusive · remote access",
            Body::Markdown(
                "Also block web tools (`web_search`, `web_fetch`) and remote (http/sse) MCP servers?"
                    .into(),
            ),
            vec![
                DialogOption::new("no", "No, keep them")
                    .key('n')
                    .describe("only model use is locked"),
                DialogOption::new("yes", "Yes, block them")
                    .key('y')
                    .describe("nothing leaves this machine except calls to the exclusive model"),
            ],
        )
        .select("no");
        self.overlay = Overlay::Dialog(
            Box::new(dialog),
            DialogPurpose::ExclusiveBlock(model, safety),
        );
    }

    fn exclusive_apply(&mut self, model: String, safety: Option<String>, block: bool) {
        let label = self.model_label(Some(&model));
        let no_safety = safety.is_none();
        match self.engine.set_exclusive(Some(crate::engine::Exclusive {
            model: model.clone(),
            safety,
            block_remote: block,
        })) {
            Ok(()) => {
                self.ctx.1 = self.catalog.get(&model).and_then(|m| m.context_window);
                let mut text = format!("exclusive: every model use goes to {label}");
                if no_safety {
                    text.push_str(" · no safety model: manual and yolo only");
                }
                if block {
                    text.push_str(" · web tools and remote MCP servers blocked");
                }
                self.transcript.notice(Level::Info, text);
            }
            Err(e) => self.transcript.notice(Level::Error, e),
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        match self.engine.set_mode(mode) {
            Ok(()) => self.hint(format!(
                "permissions: {} · {}",
                mode.as_str(),
                mode.describe()
            )),
            Err(e) => self.transcript.notice(Level::Error, e),
        }
    }

    // ---- terminal events -----------------------------------------------

    pub fn on_term(&mut self, event: Event) {
        self.dirty = true;
        self.on_term_event(event);
        if matches!(self.overlay, Overlay::None) {
            self.mention_update();
        }
    }

    fn on_term_event(&mut self, event: Event) {
        match event {
            Event::FocusGained => {
                self.attention.focused = Some(true);
                self.attention_seen();
            }
            Event::FocusLost => self.attention.focused = Some(false),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.last_key = Instant::now();
                self.attention_seen();
                self.on_key(key)
            }
            Event::Paste(text) => {
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                if let Some(search) = &mut self.search {
                    search.query.push_str(&text.replace('\n', " "));
                    search.committed = false;
                    self.search_run(true);
                    return;
                }
                if matches!(self.overlay, Overlay::None) && self.term_paste(&text) {
                    self.next_prompt();
                    return;
                }
                match &mut self.overlay {
                    Overlay::Dialog(d, _) => d.paste(&text),
                    Overlay::ForkChat(c) => c.paste(&text),
                    Overlay::Contract(ed, _) => ed.paste(&text),
                    Overlay::Questions(d) => d.paste(&text),
                    Overlay::Criteria(_)
                    | Overlay::Picker(_)
                    | Overlay::List(..)
                    | Overlay::Pager(_)
                    | Overlay::Cost(_)
                    | Overlay::Usage(_)
                    | Overlay::Diff(_)
                    | Overlay::Agents(_)
                    | Overlay::Decisions(_)
                    | Overlay::Menu(_) => {}
                    Overlay::None => {
                        self.composer.before_paste();
                        // Pasted image paths (a drag-and-drop) and an empty
                        // paste with an image on the clipboard become
                        // attachment chips.
                        if text.trim().is_empty() {
                            self.paste_clipboard_image(true);
                        } else if let Some(paths) =
                            crate::attach::pasted_images(&text, self.engine.cwd())
                        {
                            for p in paths {
                                self.attach_path(&p);
                            }
                        // Large pastes become one atomic chip.
                        } else if crate::paste::should_collapse(
                            &text,
                            self.catalog.paste_collapse_lines,
                        ) {
                            self.composer.insert_paste(&text);
                        } else {
                            self.composer.insert(&text);
                        }
                        self.composer.edited();
                    }
                }
            }
            Event::Mouse(m) => self.on_mouse(m),
            _ => {}
        }
        self.next_prompt();
    }

    /// Open a subagent's transcript (`root` returns to the root view).
    fn open_view(&mut self, id: &str) {
        self.restore_queue_edit(true);
        self.queue_focus = None;
        self.queue_scroll = 0;
        self.strip_focus = false;
        self.strip_scroll = 0;
        self.strip_sel = 0;
        self.show_finished = false;
        self.scroll = None;
        self.sel = None;
        self.focus = None;
        if id == "root" {
            self.view = None;
            return;
        }
        let Some(i) = self.tree.index(id) else {
            return self.hint("unknown agent");
        };
        if !self.tree.agents[i].loaded {
            let context = self.engine.resume_agent_context(id);
            self.tree.agents[i].ctx = (context.used, context.window);
            self.tree.agents[i].ctx_estimate = context.estimate;
            self.tree.agents[i].ctx_unknown = context.unknown;
            // Opened after a resume: replay its context files.
            let replay = self.engine.replay_agent(id, false);
            let mut tr = Transcript::default();
            tr.adopt_reasoning(&self.transcript);
            if !replay.older.is_empty() {
                tr.push_older(id, replay.older);
            }
            for items in replay.earlier {
                let mut sub = Transcript::default();
                fill(&mut sub, items, &self.tree, "root");
                tr.push_earlier(sub);
            }
            fill(&mut tr, replay.current, &self.tree, id);
            let a = &mut self.tree.agents[i];
            a.transcript = tr;
            a.loaded = true;
        }
        self.view = Some(id.to_string());
    }

    /// Ctrl+T: focus the agent strip (a list picker when there is no strip).
    fn toggle_strip_focus(&mut self) {
        if self.strip_focus {
            self.strip_focus = false;
            return;
        }
        let (_, items) = self.strip_items();
        if items.is_empty() {
            if self.view.is_some() {
                self.strip_focus = true;
                self.strip_sel = 0;
                return;
            }
            return self.hint("no subagents yet · /agents lists all agents");
        }
        self.strip_focus = true;
        self.strip_sel = self.strip_sel.min(items.len() - 1);
    }

    /// Keys while the strip has focus; false passes the key on.
    fn strip_key(&mut self, key: KeyEvent) -> bool {
        let (_, items) = self.strip_items();
        let n = items.len();
        match key.code {
            KeyCode::Up if n > 0 => self.strip_sel = self.strip_sel.saturating_sub(1),
            KeyCode::Down if n > 0 => self.strip_sel = (self.strip_sel + 1).min(n - 1),
            KeyCode::Enter => match items.get(self.strip_sel) {
                Some(view::StripItem::Child(id)) => {
                    let id = id.clone();
                    self.open_view(&id);
                    self.strip_focus = true;
                    self.strip_sel = 0;
                }
                Some(view::StripItem::Fold(_)) => self.show_finished = true,
                None => {}
            },
            KeyCode::Backspace | KeyCode::Esc => {
                // Up one level; at root, leave the strip.
                match self.view.clone() {
                    Some(id) => {
                        let parent = self
                            .tree
                            .get(&id)
                            .map(|a| a.view.parent.clone())
                            .unwrap_or_else(|| "root".into());
                        self.open_view(&parent);
                        self.strip_focus = true;
                    }
                    None => self.strip_focus = false,
                }
            }
            KeyCode::Char('x') => {
                if let Some(view::StripItem::Child(id)) = items.get(self.strip_sel) {
                    let id = id.clone();
                    self.kill_agent(&id);
                }
            }
            // Ask status, stop the branch, finalize the selected child.
            KeyCode::Char(c @ ('a' | 's' | 'f')) => {
                let id = match items.get(self.strip_sel) {
                    Some(view::StripItem::Child(id)) => Some(id.clone()),
                    _ => self.view.clone(),
                };
                if let Some(id) = id {
                    self.strip_focus = false;
                    match c {
                        'a' => self.ask_dialog(&id),
                        's' => self.stop_dialog(&id),
                        _ => self.finalize_dialog(&id),
                    }
                }
                return true;
            }
            _ => {
                self.strip_focus = false;
                return false;
            }
        }
        // Keep the selection in the strip's window.
        if self.strip_sel < self.strip_scroll {
            self.strip_scroll = self.strip_sel;
        } else if self.strip_sel >= self.strip_scroll + view::STRIP_ROWS {
            self.strip_scroll = self.strip_sel + 1 - view::STRIP_ROWS;
        }
        true
    }

    /// Kill a background subagent through its job.
    fn kill_agent(&mut self, id: &str) {
        let label = self.tree.label(id);
        let job = self.tree.agent_job(id);
        match job {
            Some(job) => {
                self.engine.kill_job(job);
                self.hint(format!("killing {label}…"));
            }
            None if id == "root" => self.hint("esc cancels root's turn"),
            None => self.hint(format!(
                "{label} is not a running background task · esc cancels root's turn and its foreground children"
            )),
        }
    }

    fn open_jobs(&mut self, select: Option<&str>) {
        let jobs = self.engine.jobs();
        for j in &jobs {
            self.tree.upsert_job(j.clone());
        }
        let mut jobs: Vec<_> = jobs.into_iter().rev().collect();
        jobs.sort_by_key(|j| !j.running());
        let rows = jobs
            .iter()
            .map(|j| {
                let what = match &j.agent {
                    Some(a) => format!("{} {}", self.tree.label(a), j.command),
                    None => j.command.clone(),
                };
                let tail = self.engine.job_tail(&j.id);
                list::Row {
                    id: j.id.clone(),
                    label: format!("{} · {what}", j.kind),
                    detail: format!(
                        "{} · {} · {}{}",
                        self.tree.label(&j.owner),
                        j.status,
                        job_age(j),
                        if tail.is_empty() {
                            String::new()
                        } else {
                            format!(" · {tail}")
                        }
                    ),
                    alert: j.status == "failed",
                }
            })
            .collect();
        let picker = ListPicker::new("Background jobs", rows)
            .select_id(select)
            .with_keys(&['x', 'r'])
            .with_footer("↑↓ select · enter output · x kill · r refresh · esc close")
            .with_empty("no background jobs in this session");
        self.overlay = Overlay::List(Box::new(picker), ListPurpose::Jobs);
    }

    /// `/mcp`: configured servers; space, enter or a click toggles one.
    fn open_mcp(&mut self, select: Option<&str>) {
        let rows = self.mcp_rows();
        let picker = ListPicker::new("MCP servers · this session", rows)
            .select_id(select)
            .with_keys(&[' '])
            .with_footer(
                "↑↓ select · space/enter/click toggle · esc close · tools are called from script",
            )
            .with_empty("no MCP servers configured (add mcps/<name>.json)");
        self.overlay = Overlay::List(Box::new(picker), ListPurpose::Mcp);
    }

    fn mcp_rows(&self) -> Vec<list::Row> {
        self.engine
            .mcp_status()
            .into_iter()
            .map(|m| {
                let mut detail = format!(
                    "{} · {} · {} tools",
                    if m.enabled { "on" } else { "off" },
                    m.state,
                    m.tools
                );
                if let Some(e) = &m.error {
                    detail.push_str(&format!(" · {}", e.lines().next().unwrap_or_default()));
                }
                if m.enabled && self.mcp_problems.contains_key(&m.name) && m.state != "connected" {
                    detail.push_str(" · ⚠ failed the startup check");
                }
                list::Row {
                    id: m.name.clone(),
                    label: format!(
                        "{} {}{}",
                        if m.enabled { "☑" } else { "☐" },
                        m.name,
                        if m.description.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", m.description)
                        }
                    ),
                    alert: m.error.is_some() || m.state == "error",
                    detail,
                }
            })
            .collect()
    }

    fn toggle_mcp(&mut self, name: &str) {
        let on = self
            .engine
            .mcp_status()
            .iter()
            .find(|m| m.name == name)
            .is_some_and(|m| m.enabled);
        match self.engine.set_mcp_enabled(name, !on) {
            Ok(()) => self.hint(format!(
                "{name} {}",
                if on { "disabled" } else { "enabled" }
            )),
            Err(e) => self.hint(e),
        }
    }

    fn open_job_output(&mut self, id: &str) {
        let Some(job) = self.tree.job(id).cloned() else {
            return;
        };
        let title = format!(
            "{} · {} · {} · {}",
            job.kind,
            truncate(job.command.lines().next().unwrap_or_default(), 50),
            self.tree.label(&job.owner),
            job.status
        );
        let text = self.engine.job_output(id);
        self.overlay = Overlay::Pager(Box::new(Pager::new(title, id, &text)));
    }

    fn model_label(&self, model: Option<&str>) -> String {
        match model {
            Some(m) => self.catalog.get(m).map_or_else(
                || m.rsplit('/').next().unwrap_or(m).to_string(),
                |i| i.label.clone(),
            ),
            None => "default model".into(),
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let total_top = self.max_top();
        let top = self.scroll.unwrap_or(total_top) as isize + delta;
        let top = top.clamp(0, total_top as isize) as usize;
        self.scroll = if top >= total_top { None } else { Some(top) };
    }

    fn max_top(&self) -> usize {
        self.layout.total.saturating_sub(self.layout.view_h)
    }

    /// Slash-menu entries for the composer's `/word`: built-ins first, then
    /// skills. A skill named like a built-in is offered as
    /// `/skill:<name>`; project skills that override global ones say so.
    fn slash_matches(&self) -> Vec<(String, String)> {
        let text = self.composer.text();
        if self.slash_dismissed || !text.starts_with('/') || text.contains(char::is_whitespace) {
            return vec![];
        }
        let word = &text[1..];
        let mut out: Vec<(String, String)> = COMMANDS
            .iter()
            .filter(|(c, _)| c.starts_with(word))
            .map(|(c, d)| (c.to_string(), d.to_string()))
            .collect();
        for s in &self.skills {
            let shadowed = COMMANDS.iter().any(|(c, _)| *c == s.name);
            let name = if shadowed {
                format!("skill:{}", s.name)
            } else {
                s.name.clone()
            };
            if !s.name.starts_with(word) && !format!("skill:{}", s.name).starts_with(word) {
                continue;
            }
            let mut d = format!(
                "skill · {}",
                s.description.lines().next().unwrap_or_default()
            );
            if s.user_only {
                d.push_str(" · you only");
            }
            if shadowed {
                d.push_str(&format!(" · /{} is built in", s.name));
            }
            if s.shadows_global {
                d.push_str(" · project skill (overrides global)");
            }
            out.push((name, d));
        }
        out
    }

    /// The skill a slash command names, if any: `/skill:<name>` always,
    /// `/<name>` unless a built-in has that name.
    fn skill_for(&self, cmd: &str) -> Option<String> {
        let name = cmd.strip_prefix("skill:").unwrap_or(cmd);
        let explicit = cmd.starts_with("skill:");
        self.skills
            .iter()
            .find(|s| s.name == name && (explicit || !COMMANDS.iter().any(|(c, _)| *c == name)))
            .map(|s| s.name.clone())
    }

    /// Run a skill: sent now, or queued while root (or the viewed child) works.
    fn run_skill(&mut self, name: &str, arg: &str) {
        if self.read_only.is_some() {
            return self.hint("read-only: /fork, /new or /resume to continue");
        }
        if self.view.is_some() || self.running {
            match self.engine.skill_message(name, arg) {
                Ok(text) => self.enqueue(text),
                Err(e) => self.transcript.notice(Level::Error, e),
            }
            return;
        }
        match self.engine.invoke_skill(name, arg) {
            Ok(()) => {
                let shown = format!("/{name} {arg}").trim_end().to_string();
                self.transcript.push(Kind::User(shown));
                self.transcript.begin_attempt();
                self.queue_edit = None;
                self.start();
                self.phase = Phase::Waiting;
            }
            Err(e) => self.transcript.notice(Level::Error, e),
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.slash_top = None;
        self.mention_top = None;
        if key.code == KeyCode::F(12) {
            return self.toggle_mouse();
        }
        // Overlays take every other key.
        match &mut self.overlay {
            Overlay::Dialog(d, _) => {
                let outcome = d.handle_key(key);
                return self.dialog_outcome(outcome);
            }
            Overlay::Picker(p) => {
                let action = p.handle_key(key);
                return self.picker_action(action);
            }
            Overlay::Questions(d) => {
                let outcome = d.handle_key(key);
                return self.question_outcome(outcome);
            }
            Overlay::ForkChat(_) => return self.fork_chat_key(key),
            Overlay::Criteria(v) => {
                let action = v.handle_key(key);
                return self.criteria_action(action);
            }
            Overlay::Contract(ed, _) => {
                let action = ed.handle_key(key);
                return self.contract_editor_action(action);
            }
            Overlay::Agents(v) => {
                let action = v.handle_key(key);
                return self.agents_action(action);
            }
            Overlay::Decisions(v) => {
                let action = v.handle_key(key);
                return self.decisions_action(action);
            }
            Overlay::Pager(p) => {
                let id = p.id.clone();
                let job = p.job;
                let full = p.full.clone();
                match p.handle_key(key) {
                    pager::Action::None => {}
                    pager::Action::Close if !job => {
                        // Back to the panel it was opened from, if any.
                        self.overlay = self.overlay_back.take().unwrap_or(Overlay::None);
                    }
                    pager::Action::Close => self.open_jobs(Some(&id)),
                    pager::Action::Edit => self.view_in_editor(full),
                    pager::Action::Refresh => {
                        let text = self.engine.job_output(&id);
                        p.set_text(&text);
                    }
                    pager::Action::Kill => {
                        if self.tree.job_running(&id) {
                            self.engine.kill_job(id);
                            self.hint("killing job…");
                        } else {
                            self.hint("job is not running");
                        }
                    }
                }
                return;
            }
            Overlay::List(l, _) => {
                let selected = l.selected_id();
                let action = l.handle_key(key);
                return self.list_action(action, selected);
            }
            Overlay::Diff(d) => {
                match d.handle_key(key) {
                    crate::diffview::Action::None => {}
                    crate::diffview::Action::Close => self.overlay = Overlay::None,
                    crate::diffview::Action::Tab(i) => self.open_diff(i),
                    crate::diffview::Action::Refresh => {
                        let tab = d.tab;
                        self.open_diff(tab);
                    }
                }
                return;
            }
            Overlay::Usage(u) => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
                    KeyCode::Char('c') if ctrl => self.overlay = Overlay::None,
                    KeyCode::Char('r') => self.engine.refresh_usage(true),
                    KeyCode::Up => u.wheel(-1),
                    KeyCode::Down => u.wheel(1),
                    KeyCode::PageUp => u.wheel(-10),
                    KeyCode::PageDown => u.wheel(10),
                    _ => {}
                }
                return;
            }
            Overlay::Cost(c) => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
                    KeyCode::Char('c') if ctrl => self.overlay = Overlay::None,
                    KeyCode::Up => c.wheel(-1),
                    KeyCode::Down => c.wheel(1),
                    KeyCode::PageUp => c.wheel(-10),
                    KeyCode::PageDown => c.wheel(10),
                    _ => {}
                }
                return;
            }
            Overlay::Menu(m) => {
                let n = m.items.len().max(1);
                match key.code {
                    KeyCode::Up => m.sel = (m.sel + n - 1) % n,
                    KeyCode::Down => m.sel = (m.sel + 1) % n,
                    KeyCode::Enter => {
                        let i = m.sel;
                        self.overlay_click_row(i);
                    }
                    _ => self.overlay = Overlay::None,
                }
                return;
            }
            Overlay::None => {}
        }
        // The terminal sidebar: typing, panel keys, ctrl+].
        if self.term_key(key) {
            return;
        }
        if self.search.is_some() {
            return self.search_key(key);
        }
        // Ctrl+F, or Cmd+F where the terminal reports Super (kitty protocol).
        if key.code == KeyCode::Char('f')
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            return self.open_search();
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && shift && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return self.copy_selection();
        }
        if alt && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            let up = key.code == KeyCode::Up;
            return if shift {
                self.jump_entry(up)
            } else {
                self.jump_prompt(up)
            };
        }
        if ctrl && key.code == KeyCode::Char('c') {
            return self.ctrl_c();
        }
        self.ctrl_c_at = None;
        if ctrl && key.code == KeyCode::Char('t') {
            return self.toggle_strip_focus();
        }
        if self.strip_focus && self.strip_key(key) {
            return;
        }
        // The queue box: ctrl+↑ selects the newest message; then its keys.
        if self.queue_key(key) {
            return;
        }
        if ctrl && key.code == KeyCode::Up && self.focus_queue() {
            return;
        }
        if self.composer.is_empty() && self.focus.is_some() && self.focus_key(key) {
            return;
        }
        // Popups and transcript selections dismiss before vim mode or clearing.
        if key.code == KeyCode::Esc {
            if self.mention_open() {
                self.mention_dismiss();
                return;
            }
            if self.sel.take().is_some() {
                return;
            }
            if !self.slash_matches().is_empty() {
                self.slash_dismissed = true;
                return;
            }
            // Insert → normal; visual/operator/pending commands dismiss first.
            if self.composer.vim_key(key) != crate::vim::Outcome::Pass {
                return;
            }
        }
        match key.code {
            KeyCode::Char('r') if ctrl => {
                let show = self.transcript.toggle_reasoning();
                self.hint(if show {
                    "reasoning shown"
                } else {
                    "reasoning hidden"
                });
                return;
            }
            KeyCode::Char('e') if ctrl => return self.cycle_effort(),
            KeyCode::BackTab => return self.cycle_mode(),
            KeyCode::PageUp => {
                return self.scroll_by(-(self.layout.view_h.saturating_sub(2).max(1) as isize));
            }
            KeyCode::PageDown => {
                return self.scroll_by(self.layout.view_h.saturating_sub(2).max(1) as isize);
            }
            KeyCode::Home if ctrl || self.composer.is_empty() => {
                self.scroll = Some(0);
                return;
            }
            KeyCode::End if ctrl || self.composer.is_empty() => {
                self.scroll = None;
                return;
            }
            KeyCode::Esc => {
                if !self.composer.is_empty() {
                    self.composer.clear();
                } else if self.running {
                    self.cancel();
                } else if self.view.is_some() {
                    self.open_view("root");
                }
                return;
            }
            _ => {}
        }
        self.composer_key(key);
    }

    fn dialog_outcome(&mut self, outcome: Outcome) {
        if matches!(outcome, Outcome::Pending) {
            return;
        }
        let Overlay::Dialog(_, purpose) = std::mem::replace(&mut self.overlay, Overlay::None)
        else {
            return;
        };
        match outcome {
            Outcome::Chosen { id, text } => self.dialog_chosen(purpose, &id, text),
            _ => self.dialog_cancelled(purpose),
        }
    }

    fn picker_action(&mut self, action: picker::Action) {
        let result = match action {
            picker::Action::None => Ok(()),
            picker::Action::Close => {
                self.overlay = Overlay::None;
                if self.exclusive_pick.take().is_some() {
                    self.hint("/exclusive cancelled");
                }
                Ok(())
            }
            picker::Action::Activate(id) if self.exclusive_pick.is_some() => {
                self.overlay = Overlay::None;
                match self.exclusive_pick.take().flatten() {
                    None => self.exclusive_ask_safety(id),
                    Some(model) => self.exclusive_ask_block(model, Some(id)),
                }
                Ok(())
            }
            picker::Action::Activate(id) => {
                self.overlay = Overlay::None;
                self.activate_model(id);
                Ok(())
            }
            picker::Action::Refresh => {
                let started = self.engine.discover(true);
                let discovering = self.engine.discovering();
                if let Overlay::Picker(p) = &mut self.overlay {
                    p.discovery_restarted(started, discovering);
                }
                Ok(())
            }
            picker::Action::ToggleModel(id) => self.engine.toggle_subagent_model(&id),
            picker::Action::ToggleProvider(p) => self.engine.toggle_subagent_provider(&p),
            picker::Action::SetSafety { id, close } => {
                let r = self.engine.set_safety_model(&id);
                if r.is_ok() {
                    self.hint(format!("safety model: {}", self.model_label(Some(&id))));
                    if close {
                        self.overlay = Overlay::None;
                    }
                }
                r
            }
        };
        if let Err(e) = result {
            self.transcript.notice(Level::Error, e);
        }
    }

    fn list_filter(&self) -> String {
        match &self.overlay {
            Overlay::List(l, _) => l.filter.clone(),
            _ => String::new(),
        }
    }

    fn list_action(&mut self, action: list::Action, selected: Option<String>) {
        let Overlay::List(_, purpose) = &self.overlay else {
            return;
        };
        let (resume, jobs, mcp) = (
            matches!(purpose, ListPurpose::Resume),
            matches!(purpose, ListPurpose::Jobs),
            matches!(purpose, ListPurpose::Mcp),
        );
        match action {
            list::Action::None if resume => self.resume_preview(),
            list::Action::None => {}
            list::Action::Close => {
                self.resume_filter.clear();
                self.overlay = Overlay::None;
            }
            list::Action::Key('\t') if resume => {
                self.resume_filter = self.list_filter();
                self.list_all = !self.list_all;
                self.open_resume();
            }
            list::Action::Key('r') if resume => {
                let Some(id) = selected else { return };
                self.resume_filter = self.list_filter();
                let title = match &self.overlay {
                    Overlay::List(l, _) => l.label_of(&id).unwrap_or_default(),
                    _ => String::new(),
                };
                let dialog = Dialog::new(
                    "Rename session",
                    Body::None,
                    vec![
                        DialogOption::new("rename", "Rename").key('r'),
                        DialogOption::new("cancel", "Cancel").key('c'),
                    ],
                )
                .with_text("new title")
                .prefill(&title)
                .text_option("rename", true)
                .needs_text(&["rename"]);
                self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::RenameSession(id));
            }
            list::Action::Key('d') if resume => {
                let Some(id) = selected else { return };
                self.resume_filter = self.list_filter();
                let title = match &self.overlay {
                    Overlay::List(l, _) => l.label_of(&id).unwrap_or_default(),
                    _ => String::new(),
                };
                let dialog = Dialog::new(
                    "Delete session?",
                    Body::Markdown(format!(
                        "**{title}** (`{}`) and its whole history are removed from disk. This cannot be undone.",
                        short(&id)
                    )),
                    vec![
                        DialogOption::new("delete", "Delete").key('d').guarded(),
                        DialogOption::new("cancel", "Cancel").key('c'),
                    ],
                )
                .select("cancel");
                self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::DeleteSession(id));
            }
            list::Action::Key('x') if jobs => {
                if let Some(id) = selected {
                    if self.tree.job_running(&id) {
                        self.engine.kill_job(id);
                        self.hint("killing job…");
                    } else {
                        self.hint("job is not running");
                    }
                }
            }
            list::Action::Key('r') if jobs => self.open_jobs(selected.as_deref()),
            list::Action::Key(' ') if mcp => {
                if let Some(id) = selected {
                    self.toggle_mcp(&id);
                    self.open_mcp(Some(&id));
                }
            }
            list::Action::Key(_) => {}
            list::Action::Choose(id) => {
                let Overlay::List(_, purpose) = std::mem::replace(&mut self.overlay, Overlay::None)
                else {
                    unreachable!()
                };
                self.list_chosen(purpose, id);
            }
        }
    }

    fn overlay_click_row(&mut self, i: usize) {
        if let Overlay::Menu(m) = &self.overlay {
            let (entry, btn) = (m.entry, m.items.get(i).copied());
            self.overlay = Overlay::None;
            if let Some(b) = btn {
                self.run_button(entry, b);
            }
        }
    }

    fn ctrl_c(&mut self) {
        if self.running {
            self.cancel();
            return;
        }
        if self.queue_edit.is_some() {
            return self.restore_queue_edit(false);
        }
        if !self.composer.is_empty() {
            self.composer.discard();
            return;
        }
        if self
            .ctrl_c_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
        {
            self.ctrl_c_at = None;
            self.command("quit", "");
        } else {
            self.ctrl_c_at = Some(Instant::now());
            self.hint("press ctrl-c again to quit");
        }
    }

    fn composer_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if ctrl && key.code == KeyCode::Char('g') {
            return self.edit_composer();
        }
        // Ctrl+V: the clipboard's image as an attachment chip.
        if ctrl && key.code == KeyCode::Char('v') {
            return self.paste_clipboard_image(false);
        }
        // The `@` popup takes ↑↓, Tab and Enter.
        if self.mention_open() && !shift && !alt {
            match key.code {
                KeyCode::Up => return self.mention_move(true),
                KeyCode::Down => return self.mention_move(false),
                KeyCode::Tab | KeyCode::Enter => {
                    let i = self.mentions.sel;
                    return self.mention_accept(i);
                }
                _ => {}
            }
        }
        let matches = self.slash_matches();
        let slash_nav =
            !matches.is_empty() && matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab);
        if !slash_nav {
            match self.composer.vim_key(key) {
                crate::vim::Outcome::Pass => {}
                crate::vim::Outcome::Handled => return,
                crate::vim::Outcome::Edited => {
                    self.composer.edited();
                    self.slash_dismissed = false;
                    return;
                }
            }
        }
        let mut edited = true;
        match key.code {
            KeyCode::Enter if !(shift || alt) => {
                if !matches.is_empty() {
                    let cmd = matches[self.slash_sel.min(matches.len() - 1)].0.clone();
                    self.composer.set_text(format!("/{cmd}"));
                }
                return self.submit();
            }
            KeyCode::Tab => {
                if !matches.is_empty() {
                    let cmd = matches[self.slash_sel.min(matches.len() - 1)].0.clone();
                    self.composer.set_text(format!("/{cmd} "));
                } else {
                    edited = false;
                }
            }
            KeyCode::Up if !matches.is_empty() => {
                self.slash_sel = (self.slash_sel + matches.len() - 1) % matches.len();
                edited = false;
            }
            KeyCode::Down if !matches.is_empty() => {
                self.slash_sel = (self.slash_sel + 1) % matches.len();
                edited = false;
            }
            KeyCode::Up => {
                self.composer.history_prev();
                return;
            }
            KeyCode::Down => {
                self.composer.history_next();
                return;
            }
            _ => {
                edited = self.composer.edit_key(key);
                // A finished `@path` becomes an attachment chip.
                if edited
                    && !ctrl
                    && let KeyCode::Char(c) = key.code
                    && c.is_whitespace()
                {
                    self.convert_mentions(true);
                }
            }
        }
        if edited {
            self.composer.edited();
            self.slash_dismissed = false;
            let n = self.slash_matches().len();
            if self.slash_sel >= n {
                self.slash_sel = 0;
            }
        }
    }

    /// Turn `@path` mentions of existing files into attachment chips.
    fn convert_mentions(&mut self, complete_only: bool) {
        self.composer
            .attach_mentions(self.engine.cwd(), complete_only);
    }

    /// Insert an attachment chip for an existing file.
    fn attach_path(&mut self, path: &std::path::Path) {
        let kind = crate::attach::kind_of(path);
        self.composer.attach(kind, &path.to_string_lossy());
        self.composer.edited();
        if let Some(m) = self.active_info()
            && !crate::attach::supported(kind, &m.input)
        {
            let label = m.label.clone();
            self.hint(format!(
                "{label} can't take {kind} input · /model to switch, or remove the chip"
            ));
        }
    }

    /// Save the clipboard's image and attach it. `quiet`: an empty paste
    /// with no image is not worth a hint.
    fn paste_clipboard_image(&mut self, quiet: bool) {
        match crate::attach::clipboard_image(&self.engine.attachment_dir()) {
            Ok(path) => self.attach_path(&path),
            Err(e) if !quiet => self.hint(e),
            Err(_) => {}
        }
    }

    /// Attachments the active model can't take: an error naming them, before
    /// anything is sent.
    fn unsupported_attachments(&self) -> Option<String> {
        let blocks: Vec<_> = self
            .composer
            .attachments()
            .map(|(k, p)| (k.to_string(), p.to_string()))
            .collect();
        if blocks.is_empty() {
            return None;
        }
        let Some(m) = self.active_info() else {
            return Some("no active model to send attachments to".into());
        };
        let bad: Vec<String> = blocks
            .iter()
            .filter(|(k, _)| !crate::attach::supported(k, &m.input))
            .map(|(k, p)| {
                format!(
                    "{k} {}",
                    std::path::Path::new(p)
                        .file_name()
                        .map_or(p.clone(), |n| n.to_string_lossy().into_owned())
                )
            })
            .collect();
        if bad.is_empty() {
            return None;
        }
        let takes = if m.input.is_empty() {
            "text".to_string()
        } else {
            m.input.join(", ")
        };
        Some(format!(
            "not sent: {} takes only {takes} input, so it can't receive {}. Pick a model that takes it (/model) or remove the chip.",
            m.label,
            bad.join(", ")
        ))
    }

    fn submit(&mut self) {
        if !self.composer.is_command() {
            self.convert_mentions(false);
            if let Some(e) = self.unsupported_attachments() {
                self.transcript.notice(Level::Error, e);
                self.scroll = None;
                return;
            }
        }
        let text = self.composer.take();
        self.slash_sel = 0;
        if text.trim().is_empty() {
            // Enter on an empty composer escalates the newest queued message.
            self.escalate_last();
            return;
        }
        let trimmed = text.trim();
        // `!cmd` runs locally; the model never sees it.
        if let Some(cmd) = trimmed.strip_prefix('!') {
            if cmd.trim().is_empty() {
                return self.hint("!command runs a shell command here; the model never sees it");
            }
            return self.run_shell(cmd.trim().to_string());
        }
        if let Some(rest) = trimmed.strip_prefix('/') {
            let (cmd, arg) = rest
                .split_once(char::is_whitespace)
                .map_or((rest, ""), |(c, a)| (c, a.trim()));
            if COMMANDS.iter().any(|(c, _)| *c == cmd) {
                return self.command(cmd, arg);
            }
            if let Some(skill) = self.skill_for(cmd) {
                return self.run_skill(&skill, arg);
            }
            if let Some(name) = cmd.strip_prefix("skill:") {
                self.transcript
                    .notice(Level::Error, format!("no skill named {name}"));
                return;
            }
            if !cmd.contains('/') {
                self.transcript.notice(
                    Level::Error,
                    format!("unknown command /{cmd}; /help lists commands"),
                );
                return;
            }
        }
        if self.read_only.is_some() {
            self.composer.set_wire(&text);
            self.hint("read-only: /fork, /new or /resume to continue");
            return;
        }
        // The composer messages the open child, or root: queued while it works.
        if self.view.is_some() || self.running {
            self.enqueue(text);
        } else {
            self.queue_edit = None;
            self.send(text);
        }
    }

    // ---- commands -------------------------------------------------------

    fn command(&mut self, cmd: &str, arg: &str) {
        match cmd {
            "model" if self.engine.exclusive().is_some() => {
                self.hint("the model is locked by /exclusive · /exclusive off unlocks it");
            }
            "exclusive" => self.exclusive_command(arg),
            "model" => {
                self.catalog = self.engine.catalog();
                if self.catalog.models.is_empty() {
                    self.config_notices();
                    self.scroll = None;
                    return;
                }
                let disabled_models = self
                    .catalog
                    .models
                    .iter()
                    .filter(|m| self.engine.subagent_disabled(&m.id))
                    .map(|m| m.id.clone())
                    .collect();
                let disabled_providers = self
                    .catalog
                    .models
                    .iter()
                    .filter(|m| self.engine.provider_subagent_disabled(&m.provider))
                    .map(|m| m.provider.clone())
                    .collect();
                let catalog = self.catalog.clone();
                let active = self.engine.active_model();
                let safety = self.engine.safety_model();
                // Expired discovery caches refresh in the background.
                self.engine.discover(false);
                let picker = Picker::new(
                    catalog,
                    active,
                    disabled_models,
                    disabled_providers,
                    safety,
                    if arg == "safety" {
                        picker::Purpose::Safety
                    } else {
                        picker::Purpose::Use
                    },
                    self.engine.discovering(),
                );
                self.overlay = Overlay::Picker(Box::new(picker));
            }
            "effort" => {
                let Some(m) = self.active_info() else {
                    self.transcript.notice(Level::Error, "no active model");
                    return;
                };
                if m.efforts.is_empty() {
                    self.transcript
                        .notice(Level::Info, format!("{} has no effort levels", m.label));
                    return;
                }
                let mut options = vec![
                    DialogOption::new("default", "default").describe(
                        m.default_effort
                            .as_ref()
                            .map(|d| format!("model default ({d})"))
                            .unwrap_or_else(|| "provider default".into()),
                    ),
                ];
                options.extend(
                    m.efforts
                        .iter()
                        .map(|e| DialogOption::new(e.clone(), e.clone())),
                );
                let current = self.engine.effort().unwrap_or_else(|| "default".into());
                let dialog = Dialog::new(
                    format!("Effort · {}", m.label),
                    Body::Markdown("Applies from the next model call. Ctrl+E cycles.".into()),
                    options,
                )
                .select(&current);
                self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Effort);
            }
            "permissions" => {
                if let Some(m) = Mode::parse(arg) {
                    return self.set_mode(m);
                }
                let mut options: Vec<DialogOption> = Mode::ALL
                    .iter()
                    .map(|m| {
                        // m/a/u/y: every option has its key, like the safety row.
                        let key = m.as_str().chars().next().unwrap_or(' ');
                        let option = DialogOption::new(m.as_str(), m.as_str())
                            .key(key)
                            .describe(m.describe());
                        // Modes that run tool calls without asking the user
                        // (auto lets the safety model approve them).
                        if matches!(m, Mode::Auto | Mode::Unattended | Mode::Yolo) {
                            option.guarded()
                        } else {
                            option
                        }
                    })
                    .collect();
                let safety = self
                    .engine
                    .safety_model()
                    .map_or("none".to_string(), |id| self.model_label(Some(&id)));
                options.push(
                    DialogOption::new("safety", format!("safety model: {safety}"))
                        .key('s')
                        .describe(
                            "reviews tool calls in auto and unattended; opens the model picker",
                        ),
                );
                let dialog = Dialog::new(
                    "Permission mode",
                    Body::Markdown(
                        "Applies to this session from the next tool call. Shift+Tab cycles.".into(),
                    ),
                    options,
                )
                .select(self.engine.mode().as_str());
                self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Mode);
            }
            "compact" => self.compact((!arg.is_empty()).then(|| arg.to_string())),
            "resume" => {
                if self.running {
                    return self.hint("cancel the running turn first");
                }
                self.resume_filter.clear();
                self.open_resume();
            }
            "fork" => {
                if self.running {
                    return self.hint("cancel the running turn first");
                }
                let dialog = Dialog::new(
                    "Fork session",
                    Body::Markdown(
                        "A fork is a new session with a copy of this history; this one stays as it is."
                            .into(),
                    ),
                    vec![
                        DialogOption::new("here", "Fork here").key('h').describe("at the current point"),
                        DialogOption::new("message", "Fork at a message…")
                            .key('m')
                            .describe("before a message you pick; its text returns to the composer"),
                    ],
                );
                self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Fork);
            }
            "new" => {
                self.switch(EngineTarget::New, "new session");
            }
            "goal" => self.goal_command(arg),
            "criteria" => self.open_criteria(),
            "agents" => self.open_agents(None),
            "decisions" => self.open_decisions(),
            "subagents" => self.subagents_command(arg.trim()),
            "jobs" => self.open_jobs(None),
            "terminal" | "terminals" => self.term_toggle(),
            "mcp" => self.open_mcp(None),
            "answer" => {
                let mut any = false;
                for p in &mut self.prompts {
                    if let Prompt::Question(_, deferred)
                    | Prompt::Goal(_, deferred)
                    | Prompt::Guidance(_, deferred)
                    | Prompt::Scope(_, deferred) = p
                    {
                        *deferred = false;
                        any = true;
                    }
                }
                if !any {
                    self.hint("no questions waiting");
                } else if matches!(self.overlay, Overlay::Questions(_)) {
                    // Reopen as one dialog over all of them.
                    self.overlay = Overlay::None;
                }
            }
            "cost" => self.overlay = Overlay::Cost(CostView::default()),
            "guidance" => self.open_guidance(),
            "prune" => self.prune_command(),
            "btw" => self.open_btw(arg),
            "diff" => self.open_diff(match arg {
                "staged" | "cached" => 1,
                "turn" => 2,
                _ => 0,
            }),
            "usage" => {
                self.usage = self.engine.provider_usage();
                self.overlay = Overlay::Usage(UsageView::default());
            }
            "copy" => self.copy_selection(),
            "mouse" => self.toggle_mouse(),
            "vim" => {
                if !self.composer.toggle_vim() {
                    self.hint("vim keys off for this session");
                } else {
                    self.hint("vim keys on for this session · esc: normal mode · config.json \"vim\": true makes it the default");
                }
            }
            "rename" => {
                let title = arg.trim();
                if title.is_empty() {
                    self.composer.set_text("/rename ");
                    return self
                        .hint("/rename <title> sets the session title (stops automatic titles)");
                }
                match self.engine.rename(title) {
                    Ok(()) => {
                        self.set_title(self.engine.title());
                        self.hint(format!("session title: {title}"));
                    }
                    Err(e) => self.transcript.notice(Level::Error, e),
                }
            }
            "help" => {
                let mut help = HELP.to_string();
                if !self.skills.is_empty() {
                    help.push_str(
                        "\n\n**Skills** (`/<name> [args]` sends the skill as a message)\n",
                    );
                    for s in &self.skills {
                        let shadowed = COMMANDS.iter().any(|(c, _)| *c == s.name);
                        let mut notes = vec![];
                        if s.user_only {
                            notes.push("you only".to_string());
                        }
                        if shadowed {
                            notes.push(format!(
                                "`/{}` is built in, so use `/skill:{}`",
                                s.name, s.name
                            ));
                        }
                        if s.shadows_global {
                            notes.push("project skill, overrides the global one".into());
                        }
                        let call = if shadowed {
                            format!("skill:{}", s.name)
                        } else {
                            s.name.clone()
                        };
                        help.push_str(&format!(
                            "\n- `/{call}` {}{}",
                            s.description.lines().next().unwrap_or_default(),
                            if notes.is_empty() {
                                String::new()
                            } else {
                                format!(" _({})_", notes.join("; "))
                            }
                        ));
                    }
                }
                self.transcript.push(Kind::Info(help));
                self.scroll = None;
            }
            "quit" | "exit" => {
                let (jobs, children) = self.engine.running_work();
                let queued = self.engine.queued_count();
                let terms = self.terms.open_private();
                if !self.running && queued == 0 && jobs + children == 0 && terms == 0 {
                    self.quit = true;
                    return;
                }
                let mut body = vec![];
                if terms > 0 {
                    body.push(format!(
                        "- {terms} private terminal{} open; quitting closes {} without sharing any output.",
                        if terms == 1 { " is" } else { "s are" },
                        if terms == 1 { "it" } else { "them" }
                    ));
                }
                if self.running {
                    body.push("- A turn is running; quitting cancels it.".to_string());
                }
                if queued > 0 {
                    body.push("- Queued messages will not be sent.".to_string());
                }
                if jobs + children > 0 {
                    body.push(format!(
                        "- {}; quitting stops {}.",
                        work_text(jobs, children),
                        if jobs + children == 1 { "it" } else { "them" }
                    ));
                }
                let dialog = Dialog::new(
                    "Quit cockpit-code?",
                    Body::Markdown(body.join("\n")),
                    vec![
                        DialogOption::new("quit", "Quit").key('q').guarded(),
                        DialogOption::new("stay", "Stay").key('n'),
                    ],
                );
                self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::Quit);
            }
            _ => {}
        }
    }

    // ---- titles -----------------------------------------------------

    fn set_title(&mut self, title: Option<String>) {
        self.title = title.filter(|t| !t.trim().is_empty());
    }

    /// The terminal window title (OSC 2).
    pub fn window_title(&self) -> String {
        let base = match &self.title {
            Some(t) => format!("{} · cockpit-code", truncate(t, 60)),
            None => "cockpit-code".into(),
        };
        match &self.attention.marker {
            Some(m) => format!("● {m} · {base}"),
            None => base,
        }
    }

    /// A block cursor while vim is in a non-insert mode.
    pub fn block_cursor(&self) -> bool {
        matches!(self.overlay, Overlay::None) && self.composer.block_cursor()
    }

    // ---- $EDITOR -----------------------------------------------------------

    /// Ctrl+G: edit the whole composer (paste chips expanded) in `$EDITOR`.
    fn edit_composer(&mut self) {
        self.editor = Some(EditorReq {
            text: self.composer.editor_text(),
            target: EditTarget::Composer,
        });
    }

    /// Edit one composer paste chip in `$EDITOR`.
    pub(super) fn edit_chip(&mut self, i: usize) {
        if let Some(b) = self.composer.chip(i) {
            self.editor = Some(EditorReq {
                text: b.full.clone(),
                target: EditTarget::Chip(i, b.id.clone()),
            });
        }
    }

    /// Open a read-only copy of `text` in `$EDITOR`.
    pub(super) fn view_in_editor(&mut self, text: String) {
        self.editor = Some(EditorReq {
            text,
            target: EditTarget::View,
        });
    }

    /// The event loop ran the editor.
    pub fn editor_done(&mut self, req: EditorReq, result: Result<Option<String>, String>) {
        self.dirty = true;
        if let EditTarget::Contract(purpose, old) = req.target {
            return self.contract_edited(purpose, *old, result);
        }
        let text = match result {
            Err(e) => return self.transcript.notice(Level::Error, e),
            Ok(None) => return self.hint("the editor exited with an error; nothing changed"),
            Ok(Some(t)) => t,
        };
        match req.target {
            EditTarget::Composer => {
                self.composer.rebuild_from_editor(&text);
                self.composer.edited();
                self.hint("composer updated from the editor");
            }
            EditTarget::Chip(i, id) => {
                let Some(b) = self.composer.chip(i).filter(|b| b.id == id) else {
                    return self.hint("that paste is gone; nothing changed");
                };
                let mut text = text;
                if b.full.ends_with('\n') && !text.ends_with('\n') {
                    text.push('\n');
                }
                if text.trim().is_empty() {
                    self.composer.remove_chip(i);
                    self.hint("paste removed");
                } else {
                    self.composer.set_chip_text(i, text);
                    self.hint("paste updated");
                }
                self.composer.edited();
            }
            EditTarget::View | EditTarget::Contract(..) => {}
        }
    }

    fn dialog_cancelled(&mut self, purpose: DialogPurpose) {
        match purpose {
            DialogPurpose::Approval(id) => {
                self.approve(&id, ApprovalChoice::Deny("denied by the user".into()))
            }
            DialogPurpose::GoalDraft => self.goal_dialog_later("goal-draft"),
            DialogPurpose::Guidance(id) => {
                for p in &mut self.prompts {
                    if let Prompt::Guidance(g, deferred) = p
                        && g.id == id
                    {
                        *deferred = true;
                    }
                }
                self.hint(
                    "put off · /answer reopens it; the waiting review holds until you decide",
                );
            }
            DialogPurpose::Proposal(_, id) | DialogPurpose::Roadblock(id) => {
                self.goal_dialog_later(&id)
            }
            DialogPurpose::Scope(id) => {
                for p in &mut self.prompts {
                    if let Prompt::Scope(v, deferred) = p
                        && v.id == id
                    {
                        *deferred = true;
                    }
                }
                self.hint("put off · /answer reopens it; the request stays pending");
            }
            DialogPurpose::Panel(_) => self.restore_back(),
            _ => {}
        }
    }

    fn question(&self, id: &str) -> Option<QuestionView> {
        self.prompts.iter().find_map(|p| match p {
            Prompt::Question(q, _) if q.id == id => Some(q.clone()),
            _ => None,
        })
    }

    /// The question dialog finished with `outcome`.
    fn question_outcome(&mut self, outcome: questions::Outcome) {
        let Overlay::Questions(d) = &self.overlay else {
            return;
        };
        let ids: Vec<String> = d.call_ids();
        if !matches!(outcome, questions::Outcome::Pending) {
            self.overlay = Overlay::None;
        }
        match outcome {
            questions::Outcome::Pending => {}
            questions::Outcome::Later => {
                // Later puts off every waiting question, not only these:
                // /answer reopens them together.
                for p in &mut self.prompts {
                    if let Prompt::Question(q, deferred) = p
                        && (ids.contains(&q.id) || !q.blocking)
                    {
                        *deferred = true;
                    }
                }
                self.hint(if ids.len() > 1 {
                    "questions put off · /answer reopens them"
                } else {
                    "question put off · /answer reopens it"
                });
            }
            questions::Outcome::Jump(asker) => {
                // Navigate without answering; the dialog reopens over the new view.
                self.open_view(&asker);
                self.next_prompt();
            }
            questions::Outcome::Discuss(id) => {
                let Some(q) = self.question(&id) else { return };
                match self.engine.discuss_question(&id) {
                    Ok(()) => {
                        self.remove_prompt(&id);
                        let row = "answered: let's discuss — your next message goes to the asker";
                        match self.transcript_of(&q.asker) {
                            Some(tr) => tr.notice(Level::Answer, row),
                            None => self.transcript.notice(Level::Answer, row),
                        }
                        self.open_view(&q.asker);
                        let who = self.tree.label(&q.asker);
                        self.hint(format!(
                            "chat with {who}: your next message is its next input"
                        ));
                    }
                    Err(e) => self.transcript.notice(Level::Error, e),
                }
            }
            questions::Outcome::ForkChat(id) => {
                let Some(q) = self.question(&id) else { return };
                let who = self.tree.label(&q.asker);
                let mut chat = ForkChat::new(who, &q.questions);
                let opened = self.engine.open_fork_chat(&id);
                let earlier = self.earlier_snapshots(&opened);
                chat.connect(opened, earlier);
                // Defer the question while chatting: the fork answers it.
                for p in &mut self.prompts {
                    if let Prompt::Question(x, deferred) = p
                        && x.id == id
                    {
                        *deferred = true;
                    }
                }
                self.overlay = Overlay::ForkChat(Box::new(chat));
            }
            questions::Outcome::Submit(answers) => {
                let n = answers.len();
                for (id, answers) in answers {
                    let Some(q) = self.question(&id) else {
                        continue;
                    };
                    match self.engine.answer_questions(&id, &q.questions, &answers) {
                        Ok(started) => {
                            self.remove_prompt(&id);
                            let summary = crate::engine::answers_text(&q.questions, &answers)
                                .replace('\n', " · ");
                            let row = format!("answered: {summary}");
                            if q.asker == "root" {
                                self.transcript.notice(Level::Answer, row);
                            } else if let Some(tr) = self.transcript_of(&q.asker) {
                                tr.notice(Level::Answer, row);
                            }
                            // A whenever answer while root is idle starts a
                            // root turn carrying it; it shows as this
                            // row, not as a typed message.
                            if started && !self.running {
                                self.transcript.begin_attempt();
                                self.start();
                                self.phase = Phase::Waiting;
                            }
                        }
                        Err(e) => {
                            for p in &mut self.prompts {
                                if let Prompt::Question(q, deferred) = p
                                    && q.id == id
                                {
                                    *deferred = true;
                                }
                            }
                            self.transcript.notice(Level::Error, &e);
                            self.hint(format!("{e} · /answer to retry"));
                        }
                    }
                }
                let left = ids.len().saturating_sub(n);
                if left > 0 {
                    self.hint(format!(
                        "{left} question{} still pending · /answer",
                        if left == 1 { "" } else { "s" }
                    ));
                }
            }
        }
    }

    /// A fork-chat event: it belongs to the overlay showing that fork;
    /// with no such overlay only closure matters (the answer row comes
    /// from the committed answer).
    fn fork_chat_event(
        &mut self,
        fork: String,
        kind: String,
        text: String,
        messages: Vec<crate::engine::ForkChatMessage>,
        call: Option<crate::engine::ToolCall>,
        result: Option<crate::engine::ToolResultData>,
    ) {
        if kind == "opened"
            && let Some(queued) = self.queued_snapshots.get_mut(&fork)
        {
            *queued -= 1;
            if *queued == 0 {
                self.queued_snapshots.remove(&fork);
            }
        }
        let parked = !matches!(self.overlay, Overlay::ForkChat(_));
        let c = match (&mut self.overlay, &mut self.parked_fork) {
            (Overlay::ForkChat(c), _) | (_, Some(c)) => c,
            _ => return,
        };
        if !c.shows(&fork) {
            return;
        }
        if c.event(&kind, text, messages, call, result) {
            if parked {
                self.parked_fork = None;
            } else {
                self.overlay = Overlay::None;
            }
            self.hint("the fork answered the question · its chat stays on the answer row");
        }
    }

    /// Reopen a fork's saved chat from its "answered via fork" row.
    pub(super) fn reopen_fork(&mut self, question: &str, summary: &str) {
        let who = self.tree.label(&self.queue_agent());
        let mut chat = ForkChat::reopened(
            who,
            vec![format!(
                "answered: {}",
                summary.lines().next().unwrap_or_default()
            )],
        );
        let opened = self.engine.open_fork_chat(question);
        let earlier = self.earlier_snapshots(&opened);
        chat.connect(opened, earlier);
        self.overlay = Overlay::ForkChat(Box::new(chat));
    }

    /// An open call returned: when it opened the fork, the engine queued
    /// one snapshot for it, behind those of earlier opens of the same fork
    /// not received yet. Returns how many of those are ahead of it (the
    /// new chat passes them over).
    pub(super) fn earlier_snapshots(&mut self, opened: &Result<String, String>) -> usize {
        let Ok(fork) = opened else { return 0 };
        let queued = self.queued_snapshots.entry(fork.clone()).or_default();
        *queued += 1;
        *queued - 1
    }

    /// Keys in the fork-chat view.
    fn fork_chat_key(&mut self, key: KeyEvent) {
        let Overlay::ForkChat(c) = &mut self.overlay else {
            return;
        };
        match c.handle_key(key) {
            crate::forkchat::Action::None => {}
            crate::forkchat::Action::Close => {
                let hint = c.close_hint();
                self.overlay = Overlay::None;
                self.hint(hint);
            }
            crate::forkchat::Action::Send { fork, text } => {
                let result = match &fork {
                    Some(fork) => self.engine.fork_chat_send(fork, text.clone()),
                    None => Err("this fork chat is not connected".into()),
                };
                // The message shows when the engine echoes it (`user`).
                if let Err(e) = result
                    && let Overlay::ForkChat(c) = &mut self.overlay
                {
                    c.send_failed(&text, e);
                }
            }
        }
    }

    fn dialog_chosen(&mut self, purpose: DialogPurpose, id: &str, text: Option<String>) {
        match purpose {
            DialogPurpose::Panel(p) => self.panel_dialog_chosen(p, id, text),
            DialogPurpose::RenameSession(sid) => {
                if id == "rename"
                    && let Some(title) = text.filter(|t| !t.trim().is_empty())
                    && let Err(e) = self.engine.rename_session(&sid, &title)
                {
                    self.hint(e);
                }
                self.open_resume();
                if let Overlay::List(l, _) = &mut self.overlay {
                    let picker = std::mem::replace(&mut **l, ListPicker::new("", vec![]));
                    **l = picker.select_id(Some(&sid));
                }
                self.resume_preview();
            }
            DialogPurpose::DeleteSession(sid) => {
                if id == "delete" {
                    match self.engine.delete_session(&sid) {
                        Ok(()) => self.hint(format!("session {} deleted", short(&sid))),
                        Err(e) => self.hint(e),
                    }
                }
                self.open_resume();
            }
            DialogPurpose::Scope(rid) => self.scope_chosen(&rid, id, text),
            DialogPurpose::Prune => {
                if id == "prune" {
                    self.prune_confirmed();
                }
            }
            DialogPurpose::Viewer => {}
            DialogPurpose::TerminalClose(tid) => {
                if id == "close" {
                    self.term_close_chosen(&tid, text);
                }
            }
            DialogPurpose::Guidance(gid) => {
                let library = self
                    .prompts
                    .iter()
                    .any(|p| matches!(p, Prompt::Guidance(g, _) if g.id == gid && g.library));
                self.remove_prompt(&gid);
                let trust = id == "trust";
                match self.engine.resolve_guidance(&gid, trust) {
                    Ok(()) => self.transcript.notice(
                        Level::Info,
                        match (library, trust) {
                            (true, true) => "project guidance library file trusted (pinned to this content)",
                            (true, false) => "project guidance library file not trusted; guidance runs without it",
                            (false, true) => "project reviewer guidance trusted (pinned to this content)",
                            (false, false) => "project reviewer guidance not trusted; reviews run without it",
                        },
                    ),
                    Err(e) => self.transcript.notice(Level::Error, e),
                }
            }
            p @ (DialogPurpose::GoalDraft
            | DialogPurpose::Proposal(..)
            | DialogPurpose::Roadblock(_)
            | DialogPurpose::GoalStatus
            | DialogPurpose::GoalClear) => self.goal_dialog_chosen(p, id, text),
            DialogPurpose::Quit => {
                if id == "quit" {
                    self.cancel();
                    self.quit = true;
                }
            }
            DialogPurpose::Effort => {
                let effort = (id != "default").then(|| id.to_string());
                match self.engine.set_effort(effort) {
                    Ok(()) => self.hint(format!("effort: {id}")),
                    Err(e) => self.transcript.notice(Level::Error, e),
                }
            }
            DialogPurpose::Mode => {
                if id == "safety" {
                    return self.command("model", "safety");
                }
                if let Some(m) = Mode::parse(id) {
                    self.set_mode(m);
                }
            }
            DialogPurpose::ExclusiveSafety(model) => match id {
                "same" => self.exclusive_ask_block(model.clone(), Some(model)),
                "pick" => self.exclusive_picker(Some(model)),
                _ => self.exclusive_ask_block(model, None),
            },
            DialogPurpose::ExclusiveBlock(model, safety) => {
                self.exclusive_apply(model, safety, id == "yes")
            }
            DialogPurpose::ModelSwitch(model) => match id {
                "switch" => self.apply_model(model),
                "compact" => {
                    self.pending_switch = Some(model);
                    self.compact(None);
                    if !self.compacting {
                        self.pending_switch = None;
                    } else {
                        self.hint("compacting with the current model, then switching");
                    }
                }
                _ => self.hint("model unchanged"),
            },
            DialogPurpose::Fork => {
                if id == "here" {
                    let source = self.engine.session_id();
                    self.switch(EngineTarget::Fork(source, None), "forked to");
                } else {
                    self.open_fork_at();
                }
            }
            DialogPurpose::Switch(target, how, refill) => {
                if id == "go" {
                    self.switch_now(target, &how, refill);
                }
            }
            DialogPurpose::Lock(info) => match id {
                "fork" => {
                    self.switch(EngineTarget::Fork(info.session_id, None), "forked to");
                }
                "claim" => {
                    self.switch(EngineTarget::Claim(info.session_id), "claimed");
                }
                _ => self.hint("not resumed"),
            },
            DialogPurpose::Approval(req) if id == "jump" => {
                // Navigate without answering; the prompt reopens over the new view.
                let agent = self.prompts.iter().find_map(|p| match p {
                    Prompt::Approval(a) if a.id == req => Some(a.agent.clone()),
                    _ => None,
                });
                if let Some(agent) = agent {
                    self.open_view(&agent);
                }
                self.next_prompt();
            }
            DialogPurpose::Approval(req) => {
                let choice = match id {
                    "once" => ApprovalChoice::Once,
                    "session" => ApprovalChoice::Session,
                    _ => ApprovalChoice::Deny(text.unwrap_or_else(|| "denied by the user".into())),
                };
                self.approve(&req, choice);
            }
        }
    }

    fn list_chosen(&mut self, purpose: ListPurpose, id: String) {
        match purpose {
            ListPurpose::Resume => match self.engine.lock_info(&id) {
                Some(info) => self.lock_dialog(info),
                None => {
                    self.switch(EngineTarget::Resume(id), "resumed");
                }
            },
            ListPurpose::ForkAt(points) => {
                let Some((context, line, text)) = id
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| points.get(i).cloned())
                else {
                    return;
                };
                let source = self.engine.session_id();
                self.switch_refill(
                    EngineTarget::Fork(source, Some((context, line))),
                    "forked to",
                    Some(text),
                );
            }
            ListPurpose::Jobs => self.open_job_output(&id),
            ListPurpose::Mcp => {
                // Enter or a click toggles; the picker stays open.
                self.toggle_mcp(&id);
                self.open_mcp(Some(&id));
            }
        }
    }

    /// Pick a new root model. While the current model's prompt cache is
    /// probably warm, ask first.
    fn activate_model(&mut self, id: String) {
        let Some(m) = self.catalog.get(&id).cloned() else {
            return;
        };
        let current = self.engine.active_model();
        if current.as_deref() == Some(id.as_str()) {
            return self.hint(format!("already using {}", m.label));
        }
        let warn = self.engine.switch_warn_minutes();
        let age = self.engine.last_message_minutes();
        let has_context = self.ctx.0 > 0 || !self.transcript.user_entries().is_empty();
        if let Some(age) = age.filter(|&a| warn > 0 && a < warn as f64 && has_context) {
            let cur = self.model_label(current.as_deref());
            let tokens = self.ctx.0;
            let price = m
                .cost
                .as_ref()
                .and_then(|c| c.input)
                .map(|rate| format!(", ≈${:.2} at its input rate", tokens as f64 * rate / 1e6))
                .unwrap_or_default();
            let mins = age.round() as u64;
            let body = format!(
                "Last message was {} ago, so **{cur}** probably still has this conversation cached. Switching sends the full context to **{}** uncached (~{} tokens{price}).",
                if mins < 1 {
                    "under a minute".to_string()
                } else {
                    format!("{mins} min")
                },
                m.label,
                fmt_tokens(tokens)
            );
            let dialog = Dialog::new(
                "Switch model?",
                Body::Markdown(body),
                vec![
                    DialogOption::new("keep", format!("Keep {cur}")).key('k'),
                    DialogOption::new("compact", format!("Compact with {cur}, then switch"))
                        .key('c')
                        .guarded()
                        .describe("the summary uses the warm cache; the new model starts small"),
                    DialogOption::new("switch", "Switch anyway").key('s'),
                ],
            );
            self.overlay = Overlay::Dialog(Box::new(dialog), DialogPurpose::ModelSwitch(id));
            return;
        }
        self.apply_model(id);
    }

    fn apply_model(&mut self, id: String) {
        if let Err(e) = self.engine.set_model(id) {
            self.transcript.notice(Level::Error, e);
        }
    }

    fn model_selected(&mut self, id: String) {
        let Some(m) = self.catalog.get(&id).cloned() else {
            return;
        };
        // The meter's window changes with the model; an unknown-context
        // model clears it until its provider reports one.
        self.ctx_unknown = m.context_unknown;
        self.ctx.1 = if m.context_unknown {
            None
        } else {
            m.context_window
        };
        let when = if self.running {
            " (from the next turn)"
        } else {
            ""
        };
        self.transcript
            .notice(Level::Info, format!("model → {} · {}{when}", m.label, m.id));
    }

    fn cycle_effort(&mut self) {
        let Some(m) = self.active_info() else {
            return;
        };
        if m.efforts.is_empty() {
            let label = m.label.clone();
            self.hint(format!("{label} has no effort levels"));
            return;
        }
        let efforts = m.efforts.clone();
        let next = match self.engine.effort() {
            None => Some(efforts[0].clone()),
            Some(e) => efforts
                .iter()
                .position(|x| *x == e)
                .and_then(|i| efforts.get(i + 1).cloned()),
        };
        match self.engine.set_effort(next.clone()) {
            Ok(()) => self.hint(format!("effort: {}", next.as_deref().unwrap_or("default"))),
            Err(e) => self.transcript.notice(Level::Error, e),
        }
    }
}

/// "2 background jobs and 1 subagent are running"
/// A skill message (`<skill id=… name="N">body … Arguments: A</skill …>`)
/// as the command the user typed: `/N A`.
fn skill_invocation(text: &str) -> Option<String> {
    let after = text.trim().strip_prefix("<skill id=\"")?;
    let (_, rest) = after.split_once("\" name=\"")?;
    let (name, rest) = rest.split_once("\">")?;
    let name = name
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    let args = rest
        .rsplit_once("\nArguments: ")
        .map(|(_, a)| a.split("\n</skill id=").next().unwrap_or("").trim())
        .unwrap_or("");
    Some(format!("/{name} {args}").trim_end().to_string())
}

/// A goal request event as a prompt.
fn goal_prompt(ev: UiEvent) -> Option<GoalPrompt> {
    Some(match ev {
        UiEvent::GoalDraft(c) => GoalPrompt::Draft(c),
        UiEvent::ProposalRequest { agent, proposal } => GoalPrompt::Proposal { agent, proposal },
        UiEvent::RoadblockRequest { agent, roadblock } => {
            GoalPrompt::Roadblock { agent, roadblock }
        }
        _ => return None,
    })
}

fn work_text(jobs: usize, children: usize) -> String {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let parts: Vec<String> = [
        (jobs > 0).then(|| plural(jobs, "background job", "background jobs")),
        (children > 0).then(|| plural(children, "subagent", "subagents")),
    ]
    .into_iter()
    .flatten()
    .collect();
    format!(
        "{} {} running",
        parts.join(" and "),
        if jobs + children == 1 { "is" } else { "are" }
    )
}

/// A child → parent message as one notice line; question tags read as a question.
fn message_text(tree: &Tree, from: &str, to: &str, text: &str) -> String {
    let (from, to) = (tree.label(from), tree.label(to));
    let question = text
        .strip_prefix("<question")
        .and_then(|r| r.split_once('>'))
        .map(|(_, r)| r.strip_suffix("</question>").unwrap_or(r));
    // Structured questions arrive as JSON items: show their text.
    let question = question.map(|q| {
        serde_json::from_str::<serde_json::Value>(q.trim())
            .ok()
            .and_then(|v| {
                let items = v.as_array()?;
                let texts: Vec<String> = items
                    .iter()
                    .filter_map(|i| {
                        let text = i["text"].as_str()?;
                        let opts: Vec<&str> = i["options"]
                            .as_array()
                            .map(|o| o.iter().filter_map(|o| o["label"].as_str()).collect())
                            .unwrap_or_default();
                        Some(if opts.is_empty() {
                            text.to_string()
                        } else {
                            format!("{text} ({})", opts.join(" / "))
                        })
                    })
                    .collect();
                (!texts.is_empty()).then(|| texts.join(" · "))
            })
            .unwrap_or_else(|| q.to_string())
    });
    match question {
        Some(q) => format!("{from} asks {to}: {q}"),
        None => format!("{from} → {to}: {text}"),
    }
}

/// The body of a user message that is only `<system-message id="R">`
/// blocks (engine-injected), joined; None for typed text.
fn system_message(text: &str) -> Option<String> {
    let mut rest = text.trim();
    let mut out = vec![];
    while !rest.is_empty() {
        let after = rest.strip_prefix("<system-message id=\"")?;
        let q = after.find("\">")?;
        let id = &after[..q];
        let close = format!("</system-message id=\"{id}\">");
        let body_end = after.find(&close)?;
        out.push(after[q + 2..body_end].trim().to_string());
        rest = after[body_end + close.len()..].trim_start();
    }
    (!out.is_empty()).then(|| out.join("\n"))
}

/// Link `task` rows to their child once the child is known.
fn link_tasks(t: &mut Transcript, tree: &Tree) {
    for (i, hint, follow_up) in t.unlinked_tasks() {
        let Some(id) = hint else { continue };
        if let Some(link) = tree.link(&id, follow_up) {
            t.link_task(i, link);
        }
    }
}

/// Build transcript entries from replayed session items.
fn fill(t: &mut Transcript, items: Vec<ReplayItem>, tree: &Tree, agent: &str) {
    for item in items {
        match item {
            ReplayItem::Tree(row) => tree_rows::fill_tree(t, tree, agent, &row),
            ReplayItem::CompactSuggested(pct, used, window) => {
                t.card(tree_rows::compact_card(pct, used, window))
            }
            // An engine-started turn (a question answer) carries a
            // tagged block, not typed text: a compact row.
            ReplayItem::User(text, ts) => match system_message(&text) {
                Some(body) => t.notice(Level::Answer, truncate(&body.replace('\n', " · "), 200)),
                None => {
                    t.push(Kind::User(skill_invocation(&text).unwrap_or(text)));
                    t.stamp_last(&ts);
                }
            },
            ReplayItem::Assistant {
                text,
                reasoning,
                ts,
            } => {
                if !reasoning.trim().is_empty() {
                    t.push(Kind::Reasoning(reasoning.trim().to_string()));
                }
                if !text.trim().is_empty() {
                    t.push(Kind::Assistant(text.trim().to_string()));
                    t.stamp_last(&ts);
                }
            }
            ReplayItem::ToolCall { id, name, args } => t.replayed_tool(id, name, args),
            ReplayItem::ToolResult {
                id,
                output,
                is_error,
                details,
            } => t.tool_end(&id, output, is_error, details),
            ReplayItem::ScriptCall {
                parent,
                call,
                result,
            } => {
                let status = if result.is_some() {
                    "completed"
                } else {
                    "started"
                };
                t.script_call(None, parent.as_deref(), call, result, status)
            }
            ReplayItem::Compacted(summary) => t.push(Kind::Compacted(summary)),
            ReplayItem::Notice(text) => t.notice(Level::Info, text),
            ReplayItem::Switched { from, to, reason } => {
                t.notice(Level::Warn, switch_text(&from, &to, &reason))
            }
            ReplayItem::Error(text) => t.notice(Level::Error, text),
            ReplayItem::Moved { id, reason } => {
                // The result row that follows carries the job id.
                t.moved_to_background(&id, "", &reason);
            }
            ReplayItem::ForkAnswered { id, summary } => t.push(Kind::ForkAnswer {
                question: id,
                summary,
            }),
            ReplayItem::Message { from, to, text } if from == "user" => {
                let _ = to;
                t.push(Kind::User(text))
            }
            ReplayItem::Message { from, to, text } => {
                t.notice(Level::Message, message_text(tree, &from, &to, &text))
            }
            ReplayItem::Scope(v) => t.card(engine_o::scope_card(tree, &v)),
            ReplayItem::Subscription { changes, overflow } => {
                t.card(engine_o::subscription_card(tree, &changes, overflow))
            }
            ReplayItem::Guidance(s) => t.card(engine_o::guidance_card(&s)),
            ReplayItem::Pruned(r) => t.card(engine_o::prune_card(&r)),
            ReplayItem::LocalRun(r) => t.card(engine_o::local_run_card(&r)),
            ReplayItem::Job(job) => {
                let what = match &job.agent {
                    Some(a) => format!("{} (background task)", tree.label(a)),
                    None => format!(
                        "{} `{}`",
                        job.kind,
                        truncate(job.command.lines().next().unwrap_or_default(), 60)
                    ),
                };
                let report = job
                    .report
                    .as_deref()
                    .and_then(|r| r.lines().find(|l| !l.trim().is_empty()))
                    .map(|l| format!(" · {}", l.trim()))
                    .unwrap_or_default();
                t.notice(Level::Job, format!("job {what} {}{report}", job.status));
            }
        }
    }
    t.end_replay();
    link_tasks(t, tree);
}

const HELP: &str = "**Keys**

| key | action |
|---|---|
| enter | send (queued while a turn runs, with `queueDefault`: next turn) |
| enter (empty composer) | escalate the newest queued message: next turn → steer → interrupt |
| ctrl+↑ | select a queued message: ↑↓ move, s send now, t toggle next turn/steer, e edit, x cancel, esc leave |
| shift+enter, alt+enter, ctrl+j | newline |
| ↑ / ↓ | history at the top / bottom logical line; Up first restores Esc-cleared text, then earlier session user messages; Down returns to the draft |
| esc | dismiss popups/selections first; clear and save input; on an empty box stop the running turn (Up restores cleared text) |
| ctrl+c | cancel the turn; twice when idle to quit |
| shift+tab | cycle permission mode (manual → auto → unattended → yolo) |
| ctrl+e | cycle effort |
| ctrl+r | show or hide reasoning |
| ctrl+f (cmd+f where the terminal reports Super through the kitty keyboard protocol) | find in the transcript: enter older match, shift+enter newer; ↑ / ↓ always step; after enter, k / j step too (any other key edits the query again); tab keeps the position, esc restores the view; earlier contexts on disk load as you go |
| @ | complete a path: one directory at a time (`@src/`, `@~/`, `@../`, `@/abs/`), or a fuzzy search of the project for a bare `@name`; ↑↓ select, tab/enter complete, esc close |
| !command | run a shell command in this directory; its output shows here and is never sent to the model (the viewed agent is told the command, exit code, duration and output size, and can ask you to paste it) |
| alt+↑ / alt+↓ | previous / next of your messages (anchors it: fork here · copy) |
| shift+alt+↑ / ↓ | focus the previous / next entry: ←→ pick a copy button, enter run it, ↑↓ scroll its box, space expand, esc leave |
| ctrl+t | focus the agent strip: ↑↓ select, enter open, ⌫/esc up a level, x kill a background child |
| esc (in a child, empty composer and no root turn running) | back to root's transcript |
| ctrl+shift+c, /copy | copy the chat selection |
| ctrl+g | edit the composer in $VISUAL / $EDITOR (paste chips expanded; attachments as `@path`) |
| ctrl+v | attach the clipboard's image (pasting or dropping an image path, or typing `@path` then space, attaches a file too) |
| ctrl+r (in /model) | re-run model discovery for command providers |
| /vim | vim keys in the composer: esc switches insert to normal; normal-mode esc clears input, then on an empty box stops the turn, i/a/o insert, v/V visual, d/c/y + motion or text object, p/P put |
| pgup / pgdn, wheel | scroll (the wheel scrolls a thinking/result box under the pointer first); home / end jump (composer empty) |
| F12 | toggle mouse capture: off gives the terminal's native selection |
| ctrl+] | type in the terminal sidebar; again for its panel keys (i type · s select rows and share · p make private · r revoke · c close with a note · k kill / x remove a job tab · ←→ or 1–8 tabs · < > width · pgup/pgdn scroll); again or esc back to the composer |

**Mouse**

- **Select**: drag in the chat; the text is copied on release (as plain rendered text).
- **Expand**: click a header to expand or collapse it; a `task` row or strip row opens that agent; the sticky header walks back through your prompts.
- **Your messages**: click one for fork here · copy; right-click any message for its actions.
- **Copy**: under replies copy text · copy rich · copy markdown; tables copy md · copy html; code blocks copy code; write/edit rows copy diff.
- **Chips**: a large paste becomes a chip (`[paste #1 · 120 lines · 4.1k chars]`); click it in the composer to view, edit in $EDITOR, expand or remove it, or in a sent message to view it. Attachment chips (`[image #1 · shot.png]`) open in the system viewer; a yellow one is an input the active model can't take, and sending waits until you switch models or remove it.
- **Status line**: segments open their pickers, the title renames the session, the quota badge opens /usage, and the git segment (`git main +staged ~modified ?untracked`) opens /diff.
- **Links**: URLs are terminal hyperlinks (OSC 8) and open on click; a link whose text differs shows its URL dimmed after it.
- **Scrollbars** page on a track click and drag by the thumb.
- **Clipboard**: over SSH copies use OSC 52 (plain text only; rich copies fall back to markdown or HTML source); inside tmux enable `set -g set-clipboard on`.

**Dialogs**: ↑↓ move (j/k are just letters), enter or a click chooses, an option's key or number chooses it. Options that destroy or approve something (compact, delete, allow, approve, apply, quit and the like) show `alt+<key>`: typed letters never choose them, only alt+key, or ↑↓ onto them + enter. A dialog opens on a safe option, so enter alone never destroys or approves. Question dialogs submit with alt+s / ctrl+s, or ↓ to Submit + enter. Keys typed in the moment a dialog opens are ignored.

While a child's transcript is open, the composer sends it a steering message (delivered at its next turn boundary).

**Attention**: when an approval, question, proposal, roadblock or trust prompt starts waiting, or a turn finishes, the terminal bell rings (tmux flags the window) and the window title gets a `●` marker until you come back; `attention: desktop` in config.json also sends an OSC 9 / OSC 777 desktop notification, `attention: off` disables it.

**Commands** `/model` `/effort` `/permissions` `/compact [focus]` `/resume` `/fork` `/new` `/rename <title>` `/goal [text|pause|resume|edit|clear]` `/criteria` `/agents` `/decisions` `/subagents [tree|flat]` `/jobs` `/terminal` `/mcp` `/answer` `/btw [question]` `/diff [staged|turn]` `/guidance` `/prune` `/cost` `/copy` `/mouse` `/vim` `/exclusive [model|off]` `/help` `/quit` (`/exit`)

**Scopes**: a child given `scope` dirs writes only there; a write outside asks the owner. Requests show as `⇄ scope request` rows (click for the diff); the owner answers apply/reject/grant, or escalates with a reason; only then a dialog offers apply · reject with a reason · grant scope (esc puts it off, `/answer` reopens it). Watched-path changes show as `✎` rows (who changed what; click for the diffs). The agent strip shows a viewed child's scope and watched paths.

**Agents**: `/agents` is the whole tree: filter by state, context, subtree and model (click the chips or 1–4), `/` searches titles, briefs and reports; ⏎ opens a transcript, `a` asks a child for its status (a read-only fork answers, the child is not interrupted), `f` finalizes a branch bottom-up (each agent writes a last report with tools disabled), `s` stops it. The strip offers the same for the viewed child. Rows show partial reports (`◐`, replaced in place), new descendants (`↳`), finalize progress (`◇`) and stops (`■`). When children report to an idle root, a `↻ children reported` row explains why a turn started; after esc, results wait (status bar) until you send a message. `/subagents` picks tree or flat (one layer) before the first message; the mode shows in the session header.

**Terminals**: when an agent asks you to do something in a terminal (enter a password, log in), the sidebar opens on the right (at 100+ columns beside the chat, drag its left edge to resize; narrower it covers the chat above the composer) and the bell rings. The header shows who asked, the prompt, the proposed command (typed, not run), the cwd and the sharing mode: **status only** (the agent gets the exit status and your note) or **output shared on close** (the last 64 KiB). You can always make it more private, or select rows (drag, or `s` then ↑↓) and stage just that excerpt. Nothing is sent before you close it; keystrokes are never recorded, and the screen is discarded on close. The terminal takes keys only after ctrl+] or a click (it takes focus by itself only when you are idle, never mid-typing). Selection there never copies to the clipboard. Tty jobs agents start (`bash tty:true`) show as red **agent-visible output** tabs you can watch and type into. `/terminal` shows or hides the sidebar.

**Decisions**: choices agents made for you (scope answers, proposal and roadblock resolutions, safety verdicts, fallbacks, stops, `decide` calls) go to `/decisions`: keep closes one, change steers the deciding agent, discuss opens a private side chat. The status bar shows the open count.

**Copy**: every copy also goes out as OSC 52 (inside tmux too), besides the local clipboard and tmux's buffer, so it reaches the terminal you are looking at even over SSH. The wheel scrolls whatever is under the pointer; it never moves a selection.

**Guidance**: `✦ guidance` rows show items injected after a write (click for the text and why); `/guidance` lists every selection per agent. Off by default: `\"guidance\": {\"enabled\": true}` in config.json.

**/prune** previews the savings, warns that it breaks the prompt cache, and stubs old delegation prompts and bulky tool outputs on confirm (the originals stay on disk).

In `/resume`, typing filters the sessions (title, model, age), tab switches between this directory and all sessions, ctrl+r renames and ctrl+d deletes (a session open in another process is never deleted); the selected session's last messages show underneath.

Start flags: `--resume <id>` (a unique prefix is enough), `--continue`, `--fork <id>`, `--claim-lock`, `--mode <mode>`.";

/// Identity of a waiting prompt, to notice new ones.
fn prompt_key(p: &Prompt) -> String {
    match p {
        Prompt::Approval(a) => format!("a:{}", a.id),
        Prompt::Question(q, _) => format!("q:{}", q.id),
        Prompt::Goal(g, _) => format!("g:{}", g.id()),
        Prompt::Guidance(g, _) => format!("t:{}", g.id),
        Prompt::Scope(v, _) => format!("s:{}", v.id),
    }
}

/// An approval raised by a fork chat (its breadcrumb ends in `fork chat`).
fn fork_approval(a: &ApprovalView) -> bool {
    a.breadcrumb.ends_with("fork chat")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::{self, TestDir};
    use crate::engine::{AgentView, QAnswer, QItem, QKind, QueueClass, Queued};

    fn app() -> (TestDir, App) {
        let (dir, engine, rx) = test_support::engine();
        (dir, App::new(engine, rx, None))
    }

    fn question() -> QuestionView {
        QuestionView {
            id: "question".into(),
            asker: "root".into(),
            breadcrumb: "root".into(),
            note: None,
            blocking: true,
            questions: vec![QItem {
                id: Some("item".into()),
                text: "Why?".into(),
                kind: QKind::Text,
                options: vec![],
                multi: false,
            }],
        }
    }

    fn fork(draft: &str) -> Overlay {
        let mut chat = ForkChat::with_summary("root".into(), vec![]);
        chat.input.set(draft);
        Overlay::ForkChat(Box::new(chat))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Opens a dialog with `open`: it opens on a safe option, a plain
    /// Enter there fires no guarded one, and its rows, field and footer
    /// name the alt+key that works. Then for many seeds it types random
    /// prose into it: no guarded option is ever chosen. Its guarded options.
    fn guarded_and_safe(app: &mut App, open: impl Fn(&mut App)) -> Vec<String> {
        use crate::dialog::tests::{assert_keys_agree, assert_opens_safe};
        for check in [assert_opens_safe, assert_keys_agree] {
            app.overlay = Overlay::None;
            open(app);
            let Overlay::Dialog(d, _) = &mut app.overlay else {
                panic!("no dialog open");
            };
            check(d);
        }
        let mut ids = vec![];
        for seed in 0..40 {
            app.overlay = Overlay::None;
            open(app);
            let Overlay::Dialog(d, _) = &mut app.overlay else {
                panic!("no dialog open");
            };
            ids = d
                .options
                .iter()
                .filter(|o| o.guarded)
                .map(|o| o.id.clone())
                .collect();
            let mut rng = crate::dialog::tests::Rng::new(seed);
            crate::dialog::tests::assert_prose_is_safe(d, &mut rng, 80);
        }
        ids
    }

    fn approval(agent: &str) -> crate::engine::ApprovalView {
        crate::engine::ApprovalView {
            call_id: "call".into(),
            parent_call_id: None,
            id: "approval".into(),
            agent: agent.into(),
            job: None,
            tool: "bash".into(),
            breadcrumb: agent.into(),
            target: None,
            diff: None,
            command: Some("rm -rf build".into()),
            reason: "cleanup".into(),
            grant_key: None,
            verdict: None,
        }
    }

    #[tokio::test]
    async fn typed_prose_chooses_no_destructive_or_approving_option() {
        let (_dir, mut app) = app();
        // Tool approvals (root's and a child's, with Jump to agent).
        for agent in ["root", "child"] {
            let ids = guarded_and_safe(&mut app, |app| {
                app.prompts.clear();
                app.prompts.push_back(Prompt::Approval(approval(agent)));
                app.next_prompt();
            });
            assert_eq!(ids, ["once", "session"], "{agent}");
        }
        for (tampered, deleted, library) in [
            (true, true, false),
            (false, false, false),
            (false, false, true),
        ] {
            let ids = guarded_and_safe(&mut app, |app| {
                app.guidance_dialog(&crate::engine::GuidanceView {
                    id: "guidance".into(),
                    path: app.engine.cwd().join("REVIEW.md"),
                    sha256: "abc".into(),
                    content: "be strict".into(),
                    tampered,
                    deleted,
                    library,
                })
            });
            assert_eq!(ids, ["trust"]);
        }
        // Claiming a live (or unknown) holder's lock; a dead one's is safe.
        for (alive, guarded) in [(Some(true), true), (None, true), (Some(false), false)] {
            let ids = guarded_and_safe(&mut app, |app| {
                app.lock_dialog(LockInfo {
                    session_id: "s".into(),
                    title: "t".into(),
                    pid: 1,
                    host: "h".into(),
                    age: "1m".into(),
                    alive,
                })
            });
            assert_eq!(ids == ["claim"], guarded, "{alive:?}");
            assert!(guarded || ids.is_empty());
        }
        let ids = guarded_and_safe(&mut app, |app| app.command("permissions", ""));
        assert_eq!(ids, ["auto", "unattended", "yolo"]);
        let ids = guarded_and_safe(&mut app, |app| app.exclusive_ask_safety("m".into()));
        assert!(ids.is_empty());
        let ids = guarded_and_safe(&mut app, |app| app.exclusive_ask_block("m".into(), None));
        assert!(ids.is_empty());
        let ids = guarded_and_safe(&mut app, |app| {
            app.running = true;
            app.command("quit", "");
        });
        assert_eq!(ids, ["quit"]);
        app.running = false;
        let ids = guarded_and_safe(&mut app, |app| {
            app.scope_dialog(&crate::engine::ScopeView {
                id: "scope".into(),
                requester: "child".into(),
                owner: "root".into(),
                path: app.engine.cwd().join("src/lib.rs"),
                tool: "write".into(),
                args: serde_json::json!({"path":"src/lib.rs", "content":"x"}),
                status: "pending".into(),
                wait: true,
                escalation: None,
                reason: None,
                output: None,
            })
        });
        assert_eq!(ids, ["apply", "grant"]);
        let ids = guarded_and_safe(&mut app, |app| {
            app.goal_prompt_dialog(&GoalPrompt::Draft(crate::engine::ContractView::default()))
        });
        assert_eq!(ids, ["approve", "cancel"]);
        let ids = guarded_and_safe(&mut app, |app| {
            app.goal_prompt_dialog(&GoalPrompt::Proposal {
                agent: "root".into(),
                proposal: crate::engine::ProposalView {
                    id: "proposal".into(),
                    criterion_id: "c1".into(),
                    change: "new text".into(),
                    reason: "because".into(),
                    ..Default::default()
                },
            })
        });
        assert_eq!(ids, ["accept", "edit"]);
        let ids = guarded_and_safe(&mut app, |app| {
            app.goal_prompt_dialog(&GoalPrompt::Roadblock {
                agent: "root".into(),
                roadblock: Default::default(),
            })
        });
        assert_eq!(ids, ["pivot", "override", "stop"]);
        let ids = guarded_and_safe(&mut app, |app| app.stop_dialog("child"));
        assert_eq!(ids, ["stop"]);
        let ids = guarded_and_safe(&mut app, |app| app.finalize_dialog("child"));
        assert_eq!(ids, ["finalize"]);
        // Asking sends a question: the prefilled field's Enter only hints.
        let ids = guarded_and_safe(&mut app, |app| app.ask_dialog("child"));
        assert_eq!(ids, ["ask"]);
        // Closing a private terminal.
        let ids = guarded_and_safe(&mut app, |app| {
            let i = app.term_add(crate::engine::TermInfo {
                id: "t1".into(),
                user: true,
                agent: "root".into(),
                breadcrumb: "root".into(),
                cwd: "/".into(),
                prompt: None,
                command: None,
                share_output: false,
                staged: 0,
                whenever: false,
                status: "running".into(),
                exit: None,
            });
            app.term_close_dialog(i);
        });
        assert_eq!(ids, ["close"]);
    }

    /// One key name per guarded action: the Quit dialog (a job running)
    /// opens on Stay and quits with alt+q; terminal close and Stop branch
    /// name alt+c and alt+s in their option, field hint and footer, and
    /// /help names no other key for them.
    #[tokio::test]
    async fn quit_opens_on_stay_and_each_guarded_action_has_one_key_name() {
        let (_dir, mut app) = app();
        app.running = true;
        app.command("quit", "");
        let Overlay::Dialog(d, DialogPurpose::Quit) = &mut app.overlay else {
            panic!("no quit dialog");
        };
        assert_eq!(d.options[d.selected].id, "stay");
        let open = Instant::now();
        assert!(matches!(
            d.key_at(key(KeyCode::Enter), open + Duration::from_millis(1000)),
            crate::dialog::Outcome::Chosen { id, .. } if id == "stay"
        ));
        app.overlay = Overlay::None;
        app.command("quit", "");
        let Overlay::Dialog(d, _) = &mut app.overlay else {
            panic!("no quit dialog");
        };
        assert!(matches!(
            d.key_at(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT), open),
            crate::dialog::Outcome::Chosen { id, .. } if id == "quit"
        ));
        app.running = false;
        let dialogs = HELP.split("**Dialogs**").nth(1).unwrap();
        let dialogs = dialogs.split("\n\n").next().unwrap();
        assert!(!dialogs.contains("alt+enter"), "{dialogs}");
        assert!(dialogs.contains("`alt+<key>`") && dialogs.contains("opens on a safe option"));
    }

    #[tokio::test]
    async fn job_and_enter_in_a_tool_approval_allow_nothing() {
        let (_dir, mut app) = app();
        app.prompts.push_back(Prompt::Approval(approval("root")));
        app.next_prompt();
        let Overlay::Dialog(d, _) = &mut app.overlay else {
            panic!("no dialog open");
        };
        // After the opening grace.
        let open = Instant::now();
        for (i, c) in "job".chars().enumerate() {
            let at = open + Duration::from_millis(1000 + 50 * i as u64);
            assert!(matches!(
                d.key_at(key(KeyCode::Char(c)), at),
                crate::dialog::Outcome::Pending
            ));
        }
        assert!(matches!(
            d.key_at(key(KeyCode::Enter), open + Duration::from_millis(1200)),
            crate::dialog::Outcome::Pending
        ));
        // Through the app too: the dialog stays and nothing is answered.
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            app.overlay,
            Overlay::Dialog(_, DialogPurpose::Approval(_))
        ));
    }

    #[test]
    fn short_ids_use_characters() {
        assert_eq!(short("界界界"), "界界界");
        assert_eq!(short("界界界界界界界界界"), "界界界界界界界界");
        assert_eq!(short("123456789"), "12345678");
    }

    #[tokio::test]
    async fn paste_leaves_visual_mode_before_multibyte_insert_and_yank() {
        let (_dir, mut app) = app();
        app.composer.enable_vim();
        app.composer.set_text("ab");
        for code in [KeyCode::Esc, KeyCode::Char('v'), KeyCode::Char('h')] {
            app.on_key(key(code));
        }
        assert_eq!(app.composer.vim_label().as_deref(), Some("-- VISUAL --"));
        app.on_term_event(Event::Paste("é".into()));
        assert_eq!(app.composer.text(), "éab");
        assert_eq!(app.composer.vim_label().as_deref(), Some("NORMAL"));
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.composer.wire(), "éab");
        assert_eq!(app.composer.selection(), None);
    }

    #[tokio::test]
    async fn session_load_drops_old_shell_receivers() {
        let (_dir, mut app) = app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.shells.push(shell::Shell {
            id: "old-shell".into(),
            agent: "root".into(),
            rx,
        });
        let (_, events) = tokio::sync::mpsc::unbounded_channel();
        app.load_session(events, "new");
        assert!(app.shells.is_empty());
        assert!(
            tx.send((
                String::new(),
                false,
                crate::engine::LocalRun {
                    command: "old command".into(),
                    exit_code: Some(0),
                    duration_ms: 0,
                    output_bytes: 0,
                }
            ))
            .is_err()
        );
        assert!(!app.poll_shells());
    }

    #[tokio::test]
    async fn root_and_child_partial_events_preserve_committed_reply() {
        let (_dir, mut app) = app();
        app.tree.add(
            AgentView {
                id: "child".into(),
                parent: "root".into(),
                role: "worker".into(),
                model: None,
                status: "running".into(),
                task: String::new(),
                background: false,
                created: String::new(),
                call_id: None,
            },
            true,
        );
        for status in ["max_tokens", "pause_turn"] {
            for child in [false, true] {
                for event in [
                    UiEvent::Text("committed".into()),
                    UiEvent::Partial {
                        status: status.into(),
                        text: "committed".into(),
                    },
                    UiEvent::Text("retry".into()),
                    UiEvent::StreamReset,
                ] {
                    if child {
                        app.child_event("child", event);
                    } else {
                        app.on_engine_event(Some(event));
                    }
                }
                let tr = if child {
                    &app.tree.get("child").unwrap().transcript
                } else {
                    &app.transcript
                };
                assert!(
                    tr.kinds()
                        .any(|k| matches!(k, Kind::Assistant(t) if t == "committed"))
                );
                assert!(
                    !tr.kinds()
                        .any(|k| matches!(k, Kind::Assistant(t) if t == "retry"))
                );
            }
        }
    }

    #[tokio::test]
    async fn root_and_child_replies_in_one_turn_stay_apart() {
        let (_dir, mut app) = app();
        app.tree.add(
            AgentView {
                id: "child".into(),
                parent: "root".into(),
                role: "worker".into(),
                model: None,
                status: "running".into(),
                task: String::new(),
                background: false,
                created: String::new(),
                call_id: None,
            },
            true,
        );
        for child in [false, true] {
            for event in [
                UiEvent::Text("done.".into()),
                UiEvent::ReplyEnd,
                UiEvent::Text("done.".into()),
            ] {
                if child {
                    app.child_event("child", event);
                } else {
                    app.on_engine_event(Some(event));
                }
            }
            let tr = if child {
                &app.tree.get("child").unwrap().transcript
            } else {
                &app.transcript
            };
            let replies: Vec<_> = tr
                .kinds()
                .filter_map(|k| match k {
                    Kind::Assistant(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(replies, ["done.", "done."]);
        }
    }

    #[tokio::test]
    async fn blocking_question_parks_fork_draft_until_deferred_or_answered() {
        for defer in [true, false] {
            let (_dir, mut app) = app();
            app.overlay = fork("unsent 界");
            app.on_engine_event(Some(UiEvent::Question(question())));
            assert!(app.parked_fork.is_some());
            app.next_prompt();
            assert!(matches!(app.overlay, Overlay::Questions(_)));
            if defer {
                app.question_outcome(questions::Outcome::Later);
            } else {
                app.on_engine_event(Some(UiEvent::Answered("question".into())));
            }
            app.next_prompt();
            let Overlay::ForkChat(chat) = &app.overlay else {
                panic!("fork not restored")
            };
            assert_eq!(chat.input.text(), "unsent 界");
            assert!(app.parked_fork.is_none());
        }
    }

    #[tokio::test]
    async fn paste_search_takes_priority_and_refreshes_matches() {
        let (_dir, mut app) = app();
        app.transcript.append_text("find 界 here");
        app.overlay = fork("keep draft");
        app.layout.area.width = 80;
        app.open_search();
        app.search.as_mut().unwrap().committed = true;
        app.on_term_event(Event::Paste("界\r\nhere".into()));
        let search = app.search.as_ref().unwrap();
        assert_eq!(search.query, "界 here");
        assert!(!search.committed);
        assert!(!search.matches.is_empty());
        assert!(app.composer.is_empty());
        let Overlay::ForkChat(chat) = &app.overlay else {
            panic!("fork changed")
        };
        assert_eq!(chat.input.text(), "keep draft");
    }

    #[tokio::test]
    async fn a_reopened_chat_passes_over_snapshots_of_earlier_opens() {
        let (_dir, mut app) = app();
        let fork = || Ok::<String, String>("side".into());
        let said = |role: &str, text: &str| crate::engine::ForkChatMessage {
            role: role.into(),
            text: text.into(),
            ts: String::new(),
            tool_call: None,
            tool_result: None,
        };
        // /btw, esc, then /btw text before the first snapshot arrived; the
        // provider is gone, so the text bounces back with its error.
        assert_eq!(app.earlier_snapshots(&fork()), 0);
        let mut chat = ForkChat::side_chat("root".into());
        let earlier = app.earlier_snapshots(&fork());
        assert_eq!(earlier, 1);
        chat.connect_with(fork(), earlier, "still there?", |_| {
            Err("fork provider unavailable".into())
        });
        app.overlay = Overlay::ForkChat(Box::new(chat));
        for _ in 0..2 {
            app.fork_chat_event(
                "side".into(),
                "opened".into(),
                String::new(),
                vec![said("user", "q")],
                None,
                None,
            );
        }
        let Overlay::ForkChat(chat) = &app.overlay else {
            panic!("the chat closed");
        };
        let rows: Vec<String> = chat
            .transcript
            .kinds()
            .map(|k| match k {
                Kind::User(t) => format!("user {t}"),
                Kind::Notice(Level::Error, t) => format!("error {t}"),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(rows, ["user q", "error fork provider unavailable"]);
        assert_eq!(chat.input.text(), "still there?");
        assert!(app.queued_snapshots.is_empty());
    }

    #[tokio::test]
    async fn paste_reaches_question_answer_field() {
        let (_dir, mut app) = app();
        app.on_engine_event(Some(UiEvent::Question(question())));
        app.next_prompt();
        app.on_term_event(Event::Paste("é\nanswer".into()));
        let Overlay::Questions(dialog) = &app.overlay else {
            panic!("questions closed")
        };
        assert!(matches!(&dialog.answers()[0].1[0], QAnswer::Text(t) if t == "é answer"));
        assert!(app.composer.is_empty());
    }

    #[tokio::test]
    async fn failed_answer_stays_deferred_and_reopenable() {
        let (_dir, mut app) = app();
        // The UI has the question, but the engine no longer knows it.
        app.on_engine_event(Some(UiEvent::Question(question())));
        app.next_prompt();
        app.question_outcome(questions::Outcome::Submit(vec![(
            "question".into(),
            vec![QAnswer::Text("answer".into())],
        )]));
        assert!(
            app.prompts
                .iter()
                .any(|p| matches!(p, Prompt::Question(q, true) if q.id == "question"))
        );
        assert!(app.hint.as_ref().unwrap().0.contains("/answer"));
        app.next_prompt();
        assert!(matches!(app.overlay, Overlay::None));
        app.command("answer", "");
        app.next_prompt();
        assert!(matches!(app.overlay, Overlay::Questions(_)));
    }

    #[tokio::test]
    async fn queued_attachment_survives_edit_restore_and_cancel() {
        let (dir, mut app) = app();
        let source = dir.0.join("input.txt");
        std::fs::write(&source, "attachment contents").unwrap();
        let wire = crate::paste::attach_wire("input", "file", source.to_str().unwrap());
        let crate::engine::QueueOutcome::Held(id) = app
            .engine
            .queue_message("root", wire, QueueClass::NextTurn)
            .unwrap();
        app.queue_act(id, crate::ui::QAct::Edit);
        assert_eq!(app.composer.attachments().count(), 1);
        assert!(app.composer.chip(1).is_none());
        let imported = app.composer.chip(0).unwrap().full.clone();
        assert_ne!(imported, source.to_string_lossy());
        assert_eq!(
            std::fs::read_to_string(&imported).unwrap(),
            "attachment contents"
        );
        app.composer.insert(" edited");
        app.restore_queue_edit(true);
        assert!(app.composer.is_empty());
        let queued = app.engine.queued("root");
        assert_eq!(queued.len(), 1);
        assert_eq!(crate::paste::attachments(&queued[0].text).len(), 1);
        app.user_cancelled = true;
        app.finish_turn("cancelled".into());
        assert_eq!(app.composer.attachments().count(), 1);
        assert!(app.composer.chip(1).is_none());
        assert_eq!(
            std::fs::read_to_string(&app.composer.chip(0).unwrap().full).unwrap(),
            "attachment contents"
        );
        assert!(app.composer.text().contains("edited"));
        assert_eq!(app.engine.queued_count(), 0);
    }

    fn missing_agent_item(text: &str) -> Queued {
        Queued {
            id: 1,
            agent: "missing-agent".into(),
            text: text.into(),
            class: QueueClass::NextTurn,
        }
    }

    #[tokio::test]
    async fn failed_edited_enqueue_restores_wire_draft() {
        let (_dir, mut app) = app();
        let wire = crate::paste::attach_wire("input", "image", "/missing/image.png");
        app.view = Some("missing-agent".into());
        app.queue_edit = Some((missing_agent_item("old draft"), 0));
        app.enqueue(wire.clone());
        assert_eq!(
            crate::paste::attachments(&app.composer.wire()),
            crate::paste::attachments(&wire)
        );
        assert_eq!(app.engine.queued_count(), 0);
        assert!(app.queue_edit.is_none());
        assert!(!app.hint.as_ref().unwrap().0.contains("queued ·"));
    }

    #[tokio::test]
    async fn failed_queue_restore_keeps_original_or_edited_draft() {
        for edited in [false, true] {
            let (_dir, mut app) = app();
            let original = crate::paste::attach_wire("input", "image", "/original.png");
            let changed = crate::paste::attach_wire("input", "image", "/edited.png");
            app.composer.set_wire(&changed);
            app.queue_edit = Some((missing_agent_item(&original), 0));
            app.restore_queue_edit(edited);
            let expected = if edited { &changed } else { &original };
            assert_eq!(
                crate::paste::attachments(&app.composer.wire()),
                crate::paste::attachments(expected)
            );
            assert!(!app.composer.is_empty());
            assert!(!app.hint.as_ref().unwrap().0.contains("went back"));
        }
    }
}
