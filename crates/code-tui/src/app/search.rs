//! Ctrl+F (Cmd+F where the terminal reports Super) transcript search: a
//! find bar in place of the composer, matches highlighted in the visible
//! rows, Enter/↑ to the older match, Shift+Enter/↓ to the newer one (D122:
//! ↑/↓ always step; once Enter committed the query, k/j step too), Tab
//! (or a click) keeps the position, Esc restores the view. Collapsed
//! earlier contexts are searched too (a match expands its section), and
//! stepping past the oldest match loads the next context still on disk.
use super::App;
use crate::transcript::Kind;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Default)]
pub struct Search {
    pub query: String,
    /// (entry, row within the entry), oldest first.
    pub matches: Vec<(usize, usize)>,
    pub cur: Option<usize>,
    /// Enter committed the query (D122): j/k step instead of typing.
    pub committed: bool,
    /// The view to restore on Esc.
    saved_scroll: Option<usize>,
    /// Earlier sections this search expanded (collapsed again on Esc).
    opened: Vec<usize>,
    /// Waiting for an older context to load.
    pub loading: bool,
    /// The row to bring into view on the next frame.
    pub goto: Option<(usize, usize)>,
    width: usize,
}

impl App {
    pub(super) fn open_search(&mut self) {
        if self.search.is_none() {
            self.search = Some(Search {
                saved_scroll: self.scroll,
                width: self.layout.area.width as usize,
                ..Search::default()
            });
        }
    }

    /// Keys while the find bar is open (all of them).
    pub(super) fn search_key(&mut self, key: KeyEvent) {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(s) = &mut self.search else { return };
        match key.code {
            KeyCode::Esc => return self.close_search(false),
            KeyCode::Tab => return self.close_search(true),
            KeyCode::Char('c') if ctrl => return self.close_search(false),
            KeyCode::Char('f')
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
                return self.close_search(true);
            }
            KeyCode::Enter if shift => {
                s.committed = !s.query.is_empty();
                return self.search_step(false);
            }
            KeyCode::Enter => {
                s.committed = !s.query.is_empty();
                return self.search_step(true);
            }
            KeyCode::Up => return self.search_step(true),
            KeyCode::Down => return self.search_step(false),
            KeyCode::Char('k') if s.committed && !ctrl => return self.search_step(true),
            KeyCode::Char('j') if s.committed && !ctrl => return self.search_step(false),
            KeyCode::Backspace => {
                s.committed = false;
                s.query.pop();
            }
            KeyCode::Char('u') if ctrl => {
                s.committed = false;
                s.query.clear();
            }
            KeyCode::Char(c) if !ctrl => {
                // Any other key edits the query again.
                s.committed = false;
                s.query.push(c);
            }
            _ => return,
        }
        self.search_run(true);
    }

    /// Recompute matches; `jump` selects the newest one and shows it.
    pub(super) fn search_run(&mut self, jump: bool) {
        let Some(s) = &mut self.search else { return };
        let q = s.query.to_lowercase();
        let width = s.width.max(8);
        let tick = self.tick;
        let tr = match self.view.as_deref().and_then(|id| self.tree.index(id)) {
            Some(i) => &mut self.tree.agents[i].transcript,
            None => &mut self.transcript,
        };
        let mut matches = vec![];
        if !q.is_empty() {
            for (i, e) in tr.entries.iter_mut().enumerate() {
                // A collapsed earlier context is searched as if open.
                let fold = matches!(e.kind, Kind::Earlier(_)) && !e.expanded;
                if fold {
                    e.expanded = true;
                }
                let r = e.render(width, tick);
                for (j, line) in r.lines.iter().enumerate() {
                    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    if text.to_lowercase().contains(&q) {
                        matches.push((i, j));
                    }
                }
                if fold {
                    e.expanded = false;
                }
            }
        }
        s.matches = matches;
        s.cur = if s.matches.is_empty() {
            None
        } else if jump {
            Some(s.matches.len() - 1)
        } else {
            s.cur.map(|c| c.min(s.matches.len() - 1))
        };
        if jump {
            self.search_show();
        }
    }

    /// Bring the current match into view, opening its earlier section.
    fn search_show(&mut self) {
        let Some(s) = &self.search else { return };
        let Some(&(e, line)) = s.cur.and_then(|c| s.matches.get(c)) else {
            return;
        };
        let tr = self.view_transcript();
        let mut opened = None;
        if let Some(entry) = tr.entries.get_mut(e)
            && matches!(entry.kind, Kind::Earlier(_))
            && !entry.expanded
        {
            entry.expanded = true;
            opened = Some(e);
        }
        let s = self.search.as_mut().unwrap();
        if let Some(e) = opened {
            s.opened.push(e);
        }
        s.goto = Some((e, line));
    }

    /// One match older (`up`) or newer. Past the oldest, load the next
    /// context on disk and continue there.
    fn search_step(&mut self, up: bool) {
        let Some(s) = &mut self.search else { return };
        if s.matches.is_empty() && !up {
            return;
        }
        match s.cur {
            Some(c) if up && c > 0 => s.cur = Some(c - 1),
            Some(c) if !up && c + 1 < s.matches.len() => s.cur = Some(c + 1),
            _ if up => {
                if self.search_load_older() {
                    return;
                }
                let n = self.search.as_ref().map_or(0, |s| s.matches.len());
                return self.hint(if n == 0 {
                    "no matches"
                } else {
                    "oldest match · nothing older on disk"
                });
            }
            _ => return self.hint("newest match"),
        }
        self.search_show();
    }

    /// Start loading the next older context; true when one is loading.
    fn search_load_older(&mut self) -> bool {
        if self.older_job.is_some() {
            if let Some(s) = &mut self.search {
                s.loading = true;
            }
            return true;
        }
        let Some((agent, n)) = self.view_transcript().take_older() else {
            return false;
        };
        let rx = self.engine.load_context(&agent, n);
        self.older_job = Some((agent, n, rx));
        if let Some(s) = &mut self.search {
            s.loading = true;
        }
        true
    }

    /// An older context arrived (`added` entries at the top): search again
    /// and step to the newest match inside it, or keep loading.
    pub(super) fn search_loaded(&mut self, added: usize) {
        let Some(s) = &mut self.search else { return };
        if !s.loading {
            // Indices moved; keep the same match.
            let cur = s.cur.and_then(|c| s.matches.get(c)).copied();
            self.search_run(false);
            if let (Some(s), Some((e, l))) = (&mut self.search, cur) {
                s.cur = s.matches.iter().position(|m| *m == (e + added, l));
            }
            return;
        }
        s.loading = false;
        s.opened.iter_mut().for_each(|e| *e += added);
        self.search_run(false);
        let s = self.search.as_mut().unwrap();
        // Entries 0..=added hold the "on disk" row and the new section.
        match s.matches.iter().rposition(|(e, _)| *e <= added) {
            Some(c) => {
                s.cur = Some(c);
                self.search_show();
            }
            None => {
                if !self.search_load_older() {
                    self.hint("oldest match · nothing older on disk");
                }
            }
        }
    }

    /// Close the find bar: keep the position, or restore the view.
    pub(super) fn close_search(&mut self, keep: bool) {
        let Some(s) = self.search.take() else { return };
        if !keep {
            let tr = self.view_transcript();
            for e in s.opened {
                if let Some(entry) = tr.entries.get_mut(e) {
                    entry.expanded = false;
                }
            }
            self.scroll = s.saved_scroll;
        }
    }
}
