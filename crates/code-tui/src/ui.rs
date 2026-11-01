//! Shared interaction chrome: the scrollbar (adapted from FlyCockpit's
//! Cockpit), the one hover rule, and the per-frame hit registry. Every
//! clickable element records its rect while rendering; clicks and hover are
//! resolved against those rects, never by re-measuring afterwards.
use crate::rows::Btn;
use crate::theme::theme;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

/// A scrollable area, for scrollbar clicks and thumb drags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScrollId {
    Transcript,
    /// The inner box of a transcript entry.
    Box(usize),
    Strip,
    Composer,
    /// Whatever overlay is open (picker, list, pager, dialog, cost).
    Overlay,
    Goals,
    /// The queue box above the composer (D73).
    Queue,
    /// The slash command popup above the composer.
    Slash,
    /// The `@` completion popup (D118).
    Mentions,
    /// A second scrollable pane of the open overlay (the `/diff` file list).
    OverlayList,
    /// The sidebar terminal's scrollback (D144).
    Terminal,
}

/// A queued message's action chip (D73).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QAct {
    /// Its class buttons: set this class (interrupt is send now).
    Set(crate::engine::QueueClass),
    /// Keyboard `t`: next turn ↔ steer.
    Toggle,
    /// Interrupt: stop the agent and send it now (D74).
    SendNow,
    /// Back into the composer.
    Edit,
    Cancel,
}

/// What a click on a recorded rect does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A copy/fork label on transcript entry `.0`.
    Button(usize, Btn),
    /// The toggle header of transcript entry `.0` (row `.1` of the entry).
    Header(usize, usize),
    /// A `task` row: open the child.
    Task(String),
    /// A past user message: anchor it.
    User(usize),
    /// The sticky previous-prompt header: scroll entry `.0` into view.
    Sticky(usize),
    Status(StatusSeg),
    /// Breadcrumb segment or strip row: view that agent.
    Agent(String),
    /// The `+ N finished` strip row.
    Finished,
    /// A scrollbar track (thumb included).
    Scroll(ScrollId),
    /// Row `.0` of the open overlay (picker, list, dialog option, menu item).
    Row(usize),
    /// A column cell of row `.0` in the model picker (`.1`: column id).
    Cell(usize, u8),
    /// Slash autocomplete item.
    Slash(usize),
    /// A paste chip in the composer (block index).
    Chip(usize),
    /// A tab of the open overlay (question dialog).
    Tab(usize),
    /// A chip-like action button of the open overlay (`.0`: action id).
    Act(u8),
    /// A queued message row (click selects it).
    Queue(u64),
    /// A queued message's class button or action chip.
    QueueAct(u64, QAct),
    /// A URL in the transcript: open it.
    Url(String),
    /// An `@` completion item (D118).
    Mention(usize),
    /// A file location (`path`, 1-based line): open it in the pager (a
    /// finding's `file:line`, D126).
    Open(String, Option<usize>),
    /// An action chip of the agent strip for agent `.0` (D131).
    StripAct(String, StripAct),
    /// The terminal sidebar (D144, D140).
    Term(TermAct),
}

/// Terminal sidebar targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermAct {
    /// Show tab `.0`.
    Tab(usize),
    Hide,
    /// Output → status only.
    Private,
    /// Stage the selected rows as the shared excerpt.
    Share,
    /// Revoke the staged excerpt.
    Revoke,
    /// Close (asks for an optional note).
    Close,
    /// Kill a running tty job.
    Kill,
    /// Remove a finished job's tab.
    Dismiss,
    /// The screen: click focuses, drag selects rows.
    Body,
    /// The left edge: drag resizes the panel.
    Edge,
}

/// The agent strip's actions on the viewed child (D131).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripAct {
    Ask,
    Stop,
    Finalize,
    Agents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusSeg {
    Title,
    Model,
    Effort,
    Mode,
    Cost,
    /// The quota badge: opens `/usage`.
    Quota,
    Goals,
    Agents,
    /// Running background jobs: opens `/jobs`.
    Jobs,
    /// The git branch and dirty counts: opens `/diff`.
    Git,
    Pending,
    /// Open D128 decisions: opens `/decisions`.
    Decisions,
    /// Terminals (D144): shows or hides the sidebar.
    Terminals,
}

/// Scrollbar geometry recorded at render time.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollGeom {
    pub track: Rect,
    pub total: usize,
    pub view: usize,
    pub offset: usize,
}

impl ScrollGeom {
    pub fn max(&self) -> usize {
        self.total.saturating_sub(self.view)
    }
    /// Thumb (start row, length) within the track.
    pub fn thumb(&self) -> (u16, u16) {
        let h = self.track.height as usize;
        if self.total <= self.view || h == 0 {
            return (0, h as u16);
        }
        let len = (h * self.view / self.total).clamp(1, h);
        let room = h - len;
        let pos = if self.max() == 0 {
            0
        } else {
            (room * self.offset.min(self.max()) + self.max() / 2) / self.max()
        };
        (pos as u16, len as u16)
    }
    /// The offset that puts the thumb's top at track row `row` (drag).
    pub fn offset_at(&self, row: isize) -> usize {
        let h = self.track.height as usize;
        let (_, len) = self.thumb();
        let room = h.saturating_sub(len as usize);
        if room == 0 {
            return 0;
        }
        let row = row.clamp(0, room as isize) as usize;
        (row * self.max() + room / 2) / room
    }
}

#[derive(Default)]
pub struct Hits {
    pub rects: Vec<(Rect, Target)>,
    pub scrolls: Vec<(ScrollId, ScrollGeom)>,
    /// Areas the wheel scrolls (D130: the view under the pointer), topmost
    /// last. Only areas whose content can differ from their scrollbar's
    /// owner need one; everything else falls back to its pane's wheel.
    pub regions: Vec<(Rect, ScrollId)>,
    /// The open overlay's box: the wheel outside it scrolls what is under
    /// the pointer instead.
    pub modal: Option<Rect>,
}

impl Hits {
    pub fn clear(&mut self) {
        self.rects.clear();
        self.scrolls.clear();
        self.regions.clear();
        self.modal = None;
    }
    /// Record a wheel area for scroll `id`.
    pub fn region(&mut self, rect: Rect, id: ScrollId) {
        if rect.width > 0 && rect.height > 0 {
            self.regions.push((rect, id));
        }
    }
    /// The topmost wheel area at a point whose content overflows.
    pub fn region_at(&self, x: u16, y: u16) -> Option<(ScrollId, ScrollGeom)> {
        self.regions
            .iter()
            .rev()
            .filter(|(r, _)| r.contains(Position { x, y }))
            .find_map(|(_, id)| self.scroll(id).map(|g| (id.clone(), g)))
    }
    pub fn add(&mut self, rect: Rect, target: Target) {
        if rect.width > 0 && rect.height > 0 {
            self.rects.push((rect, target));
        }
    }
    /// The topmost target at a point (the last recorded wins).
    pub fn at(&self, x: u16, y: u16) -> Option<(Rect, Target)> {
        self.rects
            .iter()
            .rev()
            .find(|(r, _)| r.contains(Position { x, y }))
            .cloned()
    }
    pub fn scroll(&self, id: &ScrollId) -> Option<ScrollGeom> {
        self.scrolls.iter().find(|(i, _)| i == id).map(|(_, g)| *g)
    }
}

/// `area` minus its rightmost column, which is always reserved for the track.
pub fn content(area: Rect) -> Rect {
    Rect {
        width: area.width.saturating_sub(1),
        ..area
    }
}

/// Paint a vertical scrollbar in the rightmost column of `area` when
/// `total > view` and record it; the column stays reserved either way.
pub fn scrollbar(
    buf: &mut Buffer,
    hits: &mut Hits,
    id: ScrollId,
    area: Rect,
    total: usize,
    view: usize,
    offset: usize,
) {
    if area.width == 0 || area.height == 0 || total <= view {
        return;
    }
    let t = theme();
    let track = Rect {
        x: area.right() - 1,
        width: 1,
        ..area
    };
    let geom = ScrollGeom {
        track,
        total,
        view,
        offset: offset.min(total - view),
    };
    let (pos, len) = geom.thumb();
    for dy in 0..track.height {
        let thumb = dy >= pos && dy < pos + len;
        if let Some(cell) = buf.cell_mut((track.x, track.y + dy)) {
            cell.set_symbol(if thumb { "█" } else { "│" });
            cell.set_style(Style::default().fg(if thumb { t.accent } else { t.rule }));
        }
    }
    hits.add(track, Target::Scroll(id.clone()));
    hits.scrolls.push((id, geom));
}

/// How a hovered element is highlighted (D63, Cockpit's `chip_style`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoverKind {
    /// Buttons and chips: the wash, the accent foreground, bold.
    Chip,
    /// Row-like targets: the wash across the full row width, dim text
    /// brightened to the normal foreground.
    Row,
}

/// Cockpit's single hover/selection rule for chips (and selected rows):
/// the hover wash plus the accent foreground, bold. Never an underline.
pub fn chip_style(base: Style, on: bool) -> Style {
    if !on {
        return base;
    }
    let t = theme();
    base.bg(t.select_bg)
        .fg(t.hover_fg)
        .add_modifier(Modifier::BOLD)
        .remove_modifier(Modifier::UNDERLINED)
}

/// The one hover painter: every hover path goes through here.
pub fn hover(buf: &mut Buffer, rect: Rect, kind: HoverKind) {
    let t = theme();
    let rect = rect.intersection(buf.area);
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            let Some(cell) = buf.cell_mut((x, y)) else {
                continue;
            };
            match kind {
                HoverKind::Chip => {
                    let style = chip_style(cell.style(), true);
                    cell.set_style(style);
                }
                HoverKind::Row => {
                    // Dim and secondary text brightens; colored text keeps its color.
                    let fg = cell.fg;
                    if fg == t.dim || fg == t.fog || fg == t.rule || fg == Color::Reset {
                        cell.set_fg(t.ink);
                    }
                    cell.set_bg(t.select_bg);
                    cell.modifier.remove(Modifier::UNDERLINED);
                }
            }
        }
    }
}

/// Hover on a scrollbar brightens only its thumb.
pub fn hover_thumb(buf: &mut Buffer, geom: &ScrollGeom) {
    let t = theme();
    let (pos, len) = geom.thumb();
    let r = Rect {
        y: geom.track.y + pos,
        height: len,
        ..geom.track
    };
    buf.set_style(
        r.intersection(buf.area),
        Style::default().fg(t.ink).bg(t.select_bg),
    );
}
