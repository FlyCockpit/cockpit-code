//! D118 `@` tagging: a popup like the slash menu while the composer's word
//! at the cursor is `@…`.
//!
//! - With a slash (or `~`, `/`, `.`) it completes one directory at a time,
//!   like a shell: only the directory being typed is listed (debounced),
//!   and the listing is dropped when the popup closes.
//! - A bare `@name` runs a bounded fuzzy search of the project (gitignore
//!   aware, capped by time and files), dropped after use.
//!
//! Tab or Enter completes the selection (a directory keeps the popup open
//! for its contents); a finished `@path` followed by a space becomes an
//! attachment chip as before. Esc closes the popup.
use super::App;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Typing pause before a listing or search starts.
const DEBOUNCE: Duration = Duration::from_millis(120);
/// Fuzzy search caps.
const SEARCH_TIME: Duration = Duration::from_millis(300);
const SEARCH_FILES: usize = 20_000;
const SHOWN: usize = 50;

/// One candidate: what is inserted after `@`, what is shown, a directory.
#[derive(Clone, Debug)]
pub struct Item {
    pub insert: String,
    pub label: String,
    pub dir: bool,
}

#[derive(Default)]
pub struct Mentions {
    /// The `@` token being completed: (start of `@`, end) byte offsets.
    pub span: Option<(usize, usize)>,
    /// The query the items answer, and the query in flight.
    done_for: Option<String>,
    pending: Option<(String, Instant)>,
    rx: Option<(String, mpsc::Receiver<Vec<Item>>)>,
    pub items: Vec<Item>,
    pub sel: usize,
    /// Esc closed the popup for this token.
    dismissed: Option<usize>,
    /// The last directory listing (one directory only).
    listing: Option<(PathBuf, Vec<(String, bool)>)>,
}

/// The `@token` ending at the cursor: (start of `@`, the text after it).
fn token(text: &str, cursor: usize) -> Option<(usize, String)> {
    let before = &text[..cursor.min(text.len())];
    let start = before
        .char_indices()
        .rev()
        .take_while(|(i, c)| !c.is_whitespace() || (*i > 0 && before[..*i].ends_with('\\')))
        .last()
        .map(|(i, _)| i)?;
    let word = &before[start..];
    let after = text[cursor.min(text.len())..].chars().next();
    if !word.starts_with('@') || after.is_some_and(|c| !c.is_whitespace()) {
        return None;
    }
    Some((start, word[1..].to_string()))
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_whitespace() || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Directory mode: the typed directory part (as typed), where it is on
/// disk, and the partial name after it.
fn split_dir(q: &str, cwd: &Path) -> Option<(String, PathBuf, String)> {
    let raw = unescape(q);
    let path_like = raw.contains('/') || raw.starts_with('~') || raw.starts_with('.');
    if !path_like {
        return None;
    }
    let (typed_dir, name) = match q.rfind('/') {
        Some(i) => (&q[..=i], &q[i + 1..]),
        None if q == "~" => ("~/", ""),
        None => ("", q),
    };
    let dir_raw = unescape(typed_dir);
    let dir = if let Some(rest) = dir_raw.strip_prefix("~/") {
        home()?.join(rest)
    } else if dir_raw == "~" {
        home()?
    } else if dir_raw.starts_with('/') {
        PathBuf::from(&dir_raw)
    } else {
        cwd.join(&dir_raw)
    };
    let typed = if q == "~" {
        "~/".to_string()
    } else {
        typed_dir.to_string()
    };
    Some((typed, dir, unescape(name)))
}

fn list_dir(dir: &Path) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let dir = e.file_type().is_ok_and(|t| t.is_dir())
                        || (e.file_type().is_ok_and(|t| t.is_symlink()) && e.path().is_dir());
                    (e.file_name().to_string_lossy().into_owned(), dir)
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    out
}

/// Subsequence match score (higher is better); None when not a match.
fn fuzzy(hay: &str, needle: &str) -> Option<i64> {
    let h: Vec<char> = hay.to_lowercase().chars().collect();
    let name_start = hay.rfind('/').map_or(0, |i| hay[..i + 1].chars().count());
    let mut score = 0i64;
    let mut hi = 0;
    let mut last: Option<usize> = None;
    for nc in needle.to_lowercase().chars() {
        let found = (hi..h.len()).find(|&i| h[i] == nc)?;
        score += 10;
        if last.is_some_and(|l| l + 1 == found) {
            score += 15;
        }
        if found >= name_start {
            score += 5;
        }
        if found == 0 || matches!(h.get(found.wrapping_sub(1)), Some('/' | '_' | '-' | '.')) {
            score += 10;
        }
        last = Some(found);
        hi = found + 1;
    }
    Some(score - h.len() as i64 / 4)
}

/// The bounded project search, on a worker thread.
fn search(cwd: PathBuf, q: String) -> Vec<Item> {
    let started = Instant::now();
    let mut hits: Vec<(i64, String, bool)> = vec![];
    let walk = ignore::WalkBuilder::new(&cwd)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .max_depth(Some(12))
        .build();
    for (n, entry) in walk.flatten().enumerate() {
        if n >= SEARCH_FILES || started.elapsed() > SEARCH_TIME {
            break;
        }
        let Ok(rel) = entry.path().strip_prefix(&cwd) else {
            continue;
        };
        let rel = rel.to_string_lossy();
        if rel.is_empty() {
            continue;
        }
        if let Some(s) = fuzzy(&rel, &q) {
            let dir = entry.file_type().is_some_and(|t| t.is_dir());
            hits.push((s, rel.into_owned(), dir));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
    hits.truncate(SHOWN);
    hits.into_iter()
        .map(|(_, rel, dir)| Item {
            insert: if dir {
                format!("{}/", escape(&rel))
            } else {
                escape(&rel)
            },
            label: if dir { format!("{rel}/") } else { rel },
            dir,
        })
        .collect()
}

impl App {
    /// The composer changed or the cursor moved: track the `@` token.
    pub(super) fn mention_update(&mut self) {
        let text = self.composer.buf.text.clone();
        let cursor = self.composer.buf.cursor;
        let in_chip = self
            .composer
            .buf
            .blocks
            .iter()
            .any(|b| b.start < cursor && cursor <= b.end);
        let tok = (!text.trim_start().starts_with('/') && !in_chip)
            .then(|| token(&text, cursor))
            .flatten();
        let m = &mut self.mentions;
        let Some((start, q)) = tok else {
            // Closed: drop everything, the listing included.
            *m = Default::default();
            return;
        };
        if m.dismissed == Some(start) {
            m.span = None;
            return;
        }
        m.dismissed = None;
        m.span = Some((start, cursor));
        if m.done_for.as_deref() != Some(q.as_str())
            && m.pending.as_ref().is_none_or(|(p, _)| *p != q)
        {
            m.pending = Some((q, Instant::now()));
        }
    }

    /// Run a debounced listing or search, pick up a finished one. True
    /// when the popup changed.
    pub(super) fn mention_poll(&mut self) -> bool {
        let cwd = self.engine.cwd().to_path_buf();
        let m = &mut self.mentions;
        let mut changed = false;
        if let Some((q, rx)) = &m.rx
            && let Ok(items) = rx.try_recv()
        {
            m.done_for = Some(q.clone());
            m.items = items;
            m.sel = 0;
            m.rx = None;
            changed = true;
        }
        let Some((q, at)) = m.pending.clone() else {
            return changed;
        };
        if at.elapsed() < DEBOUNCE {
            return changed;
        }
        m.pending = None;
        if let Some((typed, dir, name)) = split_dir(&q, &cwd) {
            // One directory at a time; re-read only when it changes.
            let entries = match &m.listing {
                Some((d, e)) if *d == dir => e.clone(),
                _ => {
                    let e = list_dir(&dir);
                    m.listing = Some((dir.clone(), e.clone()));
                    e
                }
            };
            let lower = name.to_lowercase();
            let show_hidden = name.starts_with('.');
            m.items = entries
                .into_iter()
                .filter(|(n, _)| show_hidden || !n.starts_with('.'))
                .filter(|(n, _)| n.to_lowercase().starts_with(&lower))
                .take(200)
                .map(|(n, d)| {
                    let insert = format!("{typed}{}{}", escape(&n), if d { "/" } else { "" });
                    Item {
                        label: format!("{}{n}{}", unescape(&typed), if d { "/" } else { "" }),
                        insert,
                        dir: d,
                    }
                })
                .collect();
            m.done_for = Some(q);
            m.sel = 0;
            return true;
        }
        if q.is_empty() {
            m.items.clear();
            m.done_for = Some(q);
            return true;
        }
        let (tx, rx) = mpsc::channel();
        let query = q.clone();
        std::thread::spawn(move || {
            let _ = tx.send(search(cwd, query));
        });
        m.rx = Some((q, rx));
        changed
    }

    /// Whether the popup is showing (and takes ↑↓, Tab, Enter, Esc).
    pub(super) fn mention_open(&self) -> bool {
        self.mentions.span.is_some() && !self.mentions.items.is_empty()
    }

    pub(super) fn mention_move(&mut self, up: bool) {
        let n = self.mentions.items.len().max(1);
        let m = &mut self.mentions;
        m.sel = if up {
            (m.sel + n - 1) % n
        } else {
            (m.sel + 1) % n
        };
    }

    pub(super) fn mention_dismiss(&mut self) {
        let start = self.mentions.span.map(|(s, _)| s);
        self.mentions = Default::default();
        self.mentions.dismissed = start;
    }

    /// Complete item `i`: a directory continues with its contents, a file
    /// gets a trailing space (and becomes a chip).
    pub(super) fn mention_accept(&mut self, i: usize) {
        let Some((start, end)) = self.mentions.span else {
            return;
        };
        let Some(item) = self.mentions.items.get(i).cloned() else {
            return;
        };
        let ins = if item.dir {
            format!("@{}", item.insert)
        } else {
            format!("@{} ", item.insert)
        };
        let (_, e) = self.composer.buf.splice(start, end, &ins);
        self.composer.buf.cursor = e;
        self.composer.edited();
        if !item.dir {
            self.convert_mentions(true);
        }
        self.mention_update();
    }
}
