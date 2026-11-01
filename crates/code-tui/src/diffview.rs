//! `/diff`: the worktree, the index, or this turn's edits (from the
//! journal's write/edit results), as a file list with +/− counts and the
//! selected file's numbered, highlighted hunks.
use crate::diff::{self, FileDiff};
use crate::engine::FileEdit;
use crate::theme::theme;
use crate::ui::{self, Hits, ScrollId, Target};
use crate::wrap::truncate;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use std::path::Path;
use std::process::{Command, Stdio};
use unicode_width::UnicodeWidthStr;

pub const TABS: [&str; 3] = ["worktree", "staged", "this turn"];
/// Untracked files shown in full up to this size.
const UNTRACKED_BYTES: u64 = 128 * 1024;
const UNTRACKED_FILES: usize = 50;

pub enum Action {
    None,
    Close,
    /// Switch to tab `.0` (the caller loads it).
    Tab(usize),
    Refresh,
}

pub struct DiffView {
    pub tab: usize,
    pub files: Vec<FileDiff>,
    /// Why there is nothing to show (not a repository, …).
    pub empty: String,
    sel: usize,
    /// Top row of the diff pane.
    top: usize,
    view: usize,
    rows: usize,
    list_top: usize,
    /// The wheel or scrollbar set `list_top`; kept until the next key.
    list_pinned: bool,
    list_view: usize,
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().next().unwrap_or("git failed").to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Worktree changes against the index, untracked files included.
pub fn worktree(cwd: &Path) -> Result<Vec<FileDiff>, String> {
    let mut files = diff::parse_unified(&git(cwd, &["diff", "--no-color", "--no-ext-diff"])?);
    let others = git(cwd, &["ls-files", "--others", "--exclude-standard"])?;
    for path in others.lines().take(UNTRACKED_FILES) {
        let full = cwd.join(path);
        let text = std::fs::metadata(&full)
            .ok()
            .filter(|m| m.is_file() && m.len() <= UNTRACKED_BYTES)
            .and_then(|_| std::fs::read_to_string(&full).ok());
        let (rows, added, _) = match &text {
            Some(t) => diff::numbered("", t, Some((1, 1))),
            None => (vec![], 0, 0),
        };
        files.push(FileDiff {
            path: path.to_string(),
            added,
            removed: 0,
            hunks: if rows.is_empty() { vec![] } else { vec![rows] },
            note: Some(if text.is_some() {
                "untracked".into()
            } else {
                "untracked · binary or large".into()
            }),
        });
    }
    Ok(files)
}

pub fn staged(cwd: &Path) -> Result<Vec<FileDiff>, String> {
    Ok(diff::parse_unified(&git(
        cwd,
        &["diff", "--cached", "--no-color", "--no-ext-diff"],
    )?))
}

/// This turn's successful writes and edits, merged per file in order.
pub fn turn(edits: &[FileEdit], cwd: &Path) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = vec![];
    for e in edits {
        let path = e
            .path
            .strip_prefix(cwd)
            .unwrap_or(&e.path)
            .display()
            .to_string();
        let i = match files.iter().position(|f| f.path == path) {
            Some(i) => i,
            None => {
                files.push(FileDiff {
                    path,
                    added: 0,
                    removed: 0,
                    hunks: vec![],
                    note: None,
                });
                files.len() - 1
            }
        };
        for h in &e.hunks {
            let (rows, a, r) = diff::numbered(
                &h.old_text,
                &h.new_text,
                Some((h.old_start_line, h.start_line)),
            );
            files[i].added += a;
            files[i].removed += r;
            files[i].hunks.push(rows);
        }
    }
    files
}

impl DiffView {
    pub fn new(tab: usize, loaded: Result<Vec<FileDiff>, String>) -> Self {
        let (files, empty) = match loaded {
            Ok(f) => (
                f,
                match tab {
                    0 => "no changes in the worktree".to_string(),
                    1 => "nothing staged".to_string(),
                    _ => "no edits in this turn yet".to_string(),
                },
            ),
            Err(e) => (vec![], e),
        };
        Self {
            tab,
            files,
            empty,
            sel: 0,
            top: 0,
            view: 10,
            rows: 0,
            list_top: 0,
            list_pinned: false,
            list_view: 1,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        self.list_pinned = false;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.files.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::Close,
            KeyCode::Char('c') if ctrl => return Action::Close,
            KeyCode::Char(c @ '1'..='3') => return Action::Tab(c as usize - '1' as usize),
            KeyCode::Tab | KeyCode::Right => return Action::Tab((self.tab + 1) % TABS.len()),
            KeyCode::BackTab | KeyCode::Left => {
                return Action::Tab((self.tab + TABS.len() - 1) % TABS.len());
            }
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                self.sel = self.sel.saturating_sub(1);
                self.top = 0;
            }
            KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                self.sel = (self.sel + 1).min(n - 1);
                self.top = 0;
            }
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll(self.view as isize - 2),
            KeyCode::PageUp => self.scroll(-(self.view as isize - 2)),
            KeyCode::Char('d') if ctrl => self.scroll(self.view as isize / 2),
            KeyCode::Char('u') if ctrl => self.scroll(-(self.view as isize / 2)),
            KeyCode::Home | KeyCode::Char('g') => self.top = 0,
            KeyCode::End | KeyCode::Char('G') => self.top = self.rows.saturating_sub(self.view),
            _ => {}
        }
        Action::None
    }

    pub fn scroll(&mut self, delta: isize) {
        let max = self.rows.saturating_sub(self.view) as isize;
        self.top = (self.top as isize + delta).clamp(0, max.max(0)) as usize;
    }

    /// A click on file row `i`.
    pub fn click(&mut self, i: usize) {
        if i < self.files.len() {
            self.sel = i;
            self.top = 0;
        }
    }

    pub fn set_offset(&mut self, top: usize) {
        self.top = top.min(self.rows.saturating_sub(self.view));
    }

    /// The file list's top row (its wheel and scrollbar; D130: the
    /// selected file stays).
    pub fn set_list_offset(&mut self, top: usize) {
        self.list_top = top.min(self.files.len().saturating_sub(self.list_view));
        self.list_pinned = true;
    }

    pub fn render(&mut self, frame: &mut Frame, screen: Rect, hits: &mut Hits) {
        let t = theme();
        let area = Rect {
            x: screen.x + 1,
            y: screen.y + 1,
            width: screen.width.saturating_sub(2),
            height: screen.height.saturating_sub(2),
        };
        frame.render_widget(Clear, area);
        hits.modal = Some(area);
        let total_add: usize = self.files.iter().map(|f| f.added).sum();
        let total_del: usize = self.files.iter().map(|f| f.removed).sum();
        let mut title = vec![Span::styled(" Diff ", t.bold())];
        for (i, name) in TABS.iter().enumerate() {
            let style = if i == self.tab {
                ui::chip_style(Style::default(), true)
            } else {
                t.faint()
            };
            title.push(Span::styled(format!(" {} {name} ", i + 1), style));
        }
        title.push(Span::styled(
            format!(
                " {} file{} +{total_add} −{total_del} ",
                self.files.len(),
                if self.files.len() == 1 { "" } else { "s" }
            ),
            t.muted(),
        ));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent))
            .title(Line::from(title))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(t.surface));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let footer = "↑↓ file · pgup/pgdn/space scroll · tab/1-3 switch · r refresh · esc close";
        let footer_area = Rect {
            y: inner.bottom().saturating_sub(1),
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(footer, inner.width as usize),
                t.faint(),
            ))),
            footer_area,
        );
        let body = Rect {
            height: inner.height.saturating_sub(2),
            ..inner
        };
        if self.files.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(self.empty.clone(), t.faint()))),
                body,
            );
            return;
        }
        self.sel = self.sel.min(self.files.len() - 1);
        // Side by side when wide; the list above the diff when narrow.
        let wide = body.width >= 100;
        let list_w = if wide {
            (body.width / 3).clamp(24, 48)
        } else {
            body.width
        };
        let list_h = if wide {
            body.height
        } else {
            (self.files.len() as u16).min(body.height / 3).max(1)
        };
        let list = Rect {
            width: list_w,
            height: list_h,
            ..body
        };
        let diff_area = if wide {
            Rect {
                x: body.x + list_w + 2,
                width: body.width.saturating_sub(list_w + 2),
                ..body
            }
        } else {
            Rect {
                y: body.y + list_h + 1,
                height: body.height.saturating_sub(list_h + 1),
                ..body
            }
        };
        // File list.
        let rows_h = list.height as usize;
        self.list_view = rows_h.max(1);
        if self.list_pinned {
            self.list_top = self.list_top.min(self.files.len().saturating_sub(rows_h));
        } else if self.sel < self.list_top {
            self.list_top = self.sel;
        } else if self.sel >= self.list_top + rows_h {
            self.list_top = self.sel + 1 - rows_h;
        }
        let mut lines = vec![];
        for (i, f) in self
            .files
            .iter()
            .enumerate()
            .skip(self.list_top)
            .take(rows_h)
        {
            let counts = format!(" +{} −{}", f.added, f.removed);
            // The last column is the list's scrollbar track.
            let room = (list.width as usize).saturating_sub(counts.width() + 4);
            // Long paths keep their file name: `…/dir/file.rs`.
            let name = if f.path.width() > room && room > 2 {
                let tail: String = f
                    .path
                    .chars()
                    .rev()
                    .scan(0, |w, c| {
                        *w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                        (*w < room).then_some(c)
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("…{tail}")
            } else {
                f.path.clone()
            };
            let pad = room.saturating_sub(name.width());
            let style = if i == self.sel {
                ui::chip_style(Style::default(), true)
            } else {
                t.text()
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "{} {name}{}",
                        if i == self.sel { "›" } else { " " },
                        " ".repeat(pad)
                    ),
                    style,
                ),
                Span::styled(
                    format!(" +{}", f.added),
                    style.fg(t.green).remove_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" −{}", f.removed),
                    style.fg(t.red).remove_modifier(Modifier::BOLD),
                ),
            ]));
            hits.add(
                Rect {
                    y: list.y + (i - self.list_top) as u16,
                    height: 1,
                    ..list
                },
                Target::Row(i),
            );
        }
        frame.render_widget(Paragraph::new(lines), list);
        hits.region(list, ScrollId::OverlayList);
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::OverlayList,
            list,
            self.files.len(),
            rows_h,
            self.list_top,
        );
        if wide {
            for y in body.top()..body.bottom() {
                if let Some(c) = frame.buffer_mut().cell_mut((body.x + list_w + 1, y)) {
                    c.set_symbol("│").set_fg(t.rule);
                }
            }
        }
        // The selected file's hunks.
        let f = &self.files[self.sel];
        let mut out: Vec<Line<'static>> = vec![Line::from(vec![
            Span::styled(f.path.clone(), t.bold()),
            Span::styled(
                f.note
                    .as_ref()
                    .map(|n| format!("  {n}"))
                    .unwrap_or_default(),
                t.faint(),
            ),
        ])];
        let painted = diff::paint(&f.path, f.hunks.clone(), f.added, f.removed);
        let w = diff_area.width.saturating_sub(1) as usize;
        let hang = [Span::raw(" ".repeat(painted.indent))];
        out.extend(crate::wrap::wrap_lines(painted.lines, w, &[], &hang));
        if f.hunks.is_empty() && f.note.is_none() {
            out.push(Line::from(Span::styled("(no text changes)", t.faint())));
        }
        self.rows = out.len();
        self.view = diff_area.height as usize;
        self.top = self.top.min(self.rows.saturating_sub(self.view));
        let shown: Vec<Line<'static>> = out.into_iter().skip(self.top).take(self.view).collect();
        frame.render_widget(
            Paragraph::new(shown),
            Rect {
                width: diff_area.width.saturating_sub(1),
                ..diff_area
            },
        );
        ui::scrollbar(
            frame.buffer_mut(),
            hits,
            ScrollId::Overlay,
            Rect {
                x: diff_area.right().saturating_sub(1),
                width: 1,
                ..diff_area
            },
            self.rows,
            self.view,
            self.top,
        );
    }
}
