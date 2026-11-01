//! Unified diff for `write` / `edit` calls, adapted from FlyCockpit's Cockpit's
//! inline renderer (side-by-side mode and config coupling removed). Rows
//! carry dim line numbers when the engine reported where each hunk starts
//! (engine-m `FileEdit` hunks), and code is highlighted by file extension
//! on top of the +/− backgrounds.
use crate::engine::FileEdit;
use crate::highlight::Highlighter;
use crate::theme::theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};
use unicode_width::UnicodeWidthStr;

const CONTEXT_LINES: usize = 3;

pub struct Diff {
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<Line<'static>>,
    /// Columns before the code on each row (number gutter + sign), for a
    /// hanging indent when a row wraps.
    pub indent: usize,
}

/// One shown row: change, line number (new side; old side for a deletion), text.
pub type Numbered = (ChangeTag, Option<usize>, String);

/// Render `old → new` as unified rows (`+`/`-`/context) with `…` between hunks.
/// Rows are not wrapped here; the transcript wraps them with a hanging gutter.
pub fn render(old: &str, new: &str) -> Diff {
    render_file("", old, new, None)
}

/// [`render`] for a file: highlighted by `path`'s extension, numbered from
/// `start` = (old first line, new first line) when known.
pub fn render_file(path: &str, old: &str, new: &str, start: Option<(usize, usize)>) -> Diff {
    let (rows, added, removed) = numbered(old, new, start);
    paint(path, vec![rows], added, removed)
}

/// The hunks of a finished write/edit, numbered, `…` between them.
pub fn render_edits(path: &str, edits: &[FileEdit]) -> Diff {
    let mut hunks = vec![];
    let (mut added, mut removed) = (0, 0);
    for e in edits {
        for h in &e.hunks {
            let (rows, a, r) = numbered(
                &h.old_text,
                &h.new_text,
                Some((h.old_start_line, h.start_line)),
            );
            added += a;
            removed += r;
            hunks.push(rows);
        }
    }
    paint(path, hunks, added, removed)
}

/// Diff rows with line numbers, near-change rows only (`None` text = gap).
pub fn numbered(
    old: &str,
    new: &str,
    start: Option<(usize, usize)>,
) -> (Vec<Option<Numbered>>, usize, usize) {
    // Fragments rarely end in a newline; a missing one is not a change.
    let nl = |s: &str| {
        if s.is_empty() || s.ends_with('\n') {
            s.to_string()
        } else {
            format!("{s}\n")
        }
    };
    let (old, new) = (nl(old), nl(new));
    let diff = TextDiff::from_lines(&old, &new);
    let mut rows: Vec<(ChangeTag, String)> = diff
        .iter_all_changes()
        .map(|change| {
            let value = change.value();
            let value = value.strip_suffix('\n').unwrap_or(value);
            let value = value.strip_suffix('\r').unwrap_or(value);
            (change.tag(), value.replace('\t', "    "))
        })
        .collect();
    slide(&mut rows);
    let added = rows.iter().filter(|r| r.0 == ChangeTag::Insert).count();
    let removed = rows.iter().filter(|r| r.0 == ChangeTag::Delete).count();
    // Rows within CONTEXT_LINES of a change are shown; gaps become `…`.
    let mut shown = vec![false; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        if r.0 != ChangeTag::Equal {
            let lo = i.saturating_sub(CONTEXT_LINES);
            let hi = (i + CONTEXT_LINES + 1).min(rows.len());
            shown[lo..hi].iter_mut().for_each(|s| *s = true);
        }
    }
    let (mut o, mut n) = start.unwrap_or((1, 1));
    let mut out = vec![];
    let mut last: Option<usize> = None;
    for (i, (tag, value)) in rows.into_iter().enumerate() {
        let num = match tag {
            ChangeTag::Delete => o,
            _ => n,
        };
        match tag {
            ChangeTag::Delete => o += 1,
            ChangeTag::Insert => n += 1,
            ChangeTag::Equal => {
                o += 1;
                n += 1;
            }
        }
        if !shown[i] {
            continue;
        }
        if last.is_some_and(|l| l + 1 != i) {
            out.push(None);
        }
        last = Some(i);
        out.push(Some((tag, start.map(|_| num), value)));
    }
    (out, added, removed)
}

pub fn paint(path: &str, hunks: Vec<Vec<Option<Numbered>>>, added: usize, removed: usize) -> Diff {
    let t = theme();
    let numw = hunks
        .iter()
        .flatten()
        .flatten()
        .filter_map(|r| r.1)
        .max()
        .map(|m| m.to_string().len());
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_string();
    let known = Highlighter::knows(&ext);
    let add_bg = Style::default().bg(t.diff_add_bg);
    let del_bg = Style::default().bg(t.diff_del_bg);
    let gap = |numw: Option<usize>| {
        let pad = numw.map_or(0, |w| w + 1);
        Line::from(Span::styled(
            format!("{}  …", " ".repeat(pad)),
            Style::default().fg(t.dim),
        ))
    };
    let mut lines = Vec::new();
    for (h, rows) in hunks.into_iter().enumerate() {
        if h > 0 {
            lines.push(gap(numw));
        }
        // Old and new text highlight as separate streams, as they parse.
        let mut hl_old = Highlighter::new(&ext);
        let mut hl_new = Highlighter::new(&ext);
        for row in rows {
            let Some((tag, num, value)) = row else {
                lines.push(gap(numw));
                continue;
            };
            let mut spans = vec![];
            if let Some(w) = numw {
                let label = num.map(|n| n.to_string()).unwrap_or_default();
                spans.push(Span::styled(
                    format!("{label:>w$} "),
                    Style::default().fg(t.dim),
                ));
            }
            let (sign, bg, sign_fg) = match tag {
                ChangeTag::Delete => ("- ", del_bg, t.red),
                ChangeTag::Insert => ("+ ", add_bg, t.green),
                ChangeTag::Equal => ("  ", Style::default(), t.fog),
            };
            spans.push(Span::styled(sign, bg.fg(sign_fg)));
            let code = match tag {
                ChangeTag::Delete => hl_old.line(&value, bg),
                ChangeTag::Insert => hl_new.line(&value, bg),
                ChangeTag::Equal => {
                    let _ = hl_old.line(&value, bg);
                    hl_new.line(&value, bg)
                }
            };
            // Unknown languages: the row keeps the classic +/− colours.
            let code = if !known {
                vec![Span::styled(value, bg.fg(sign_fg))]
            } else {
                code
            };
            spans.extend(code);
            lines.push(Line::from(spans));
        }
    }
    let indent = numw.map_or(0, |w| w + 1) + "+ ".width();
    Diff {
        added,
        removed,
        lines,
        indent,
    }
}

/// Unified diff text (with `@@` headers) of a finished write/edit's hunks.
pub fn unified_from_edits(path: &str, edits: &[FileEdit], new_file: bool) -> String {
    let mut out = format!(
        "--- {}\n+++ b/{path}\n",
        if new_file {
            "/dev/null".to_string()
        } else {
            format!("a/{path}")
        }
    );
    for e in edits {
        for h in &e.hunks {
            let lines = |s: &str| s.lines().count();
            let (oc, nc) = (lines(&h.old_text), lines(&h.new_text));
            out.push_str(&format!(
                "@@ -{},{oc} +{},{nc} @@\n",
                h.old_start_line, h.start_line
            ));
            let nl = |s: &str| {
                if s.is_empty() || s.ends_with('\n') {
                    s.to_string()
                } else {
                    format!("{s}\n")
                }
            };
            let (o, n) = (nl(&h.old_text), nl(&h.new_text));
            for c in TextDiff::from_lines(&o, &n).iter_all_changes() {
                let sign = match c.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                };
                out.push(sign);
                out.push_str(c.value());
                if !c.value().ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Slide each run of pure insertions (or deletions) down while its first
/// line equals the context line after it, as GNU diff does: an inserted
/// method then reads as `}` kept, new lines, `}` added, instead of the
/// closing brace of the previous method looking added.
fn slide(rows: &mut [(ChangeTag, String)]) {
    let mut i = 0;
    while i < rows.len() {
        let tag = rows[i].0;
        if tag == ChangeTag::Equal {
            i += 1;
            continue;
        }
        let mut end = i;
        while end < rows.len() && rows[end].0 == tag {
            end += 1;
        }
        // Only runs bounded by context (not a replace) slide.
        let before_ok = i == 0 || rows[i - 1].0 == ChangeTag::Equal;
        let (mut a, mut b) = (i, end);
        while before_ok && b < rows.len() && rows[b].0 == ChangeTag::Equal && rows[b].1 == rows[a].1
        {
            rows[a].0 = ChangeTag::Equal;
            rows[b].0 = tag;
            a += 1;
            b += 1;
        }
        i = b.max(end);
    }
}

/// One file of a parsed unified diff (`git diff`).
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// Hunks as numbered rows (new-side numbers; old side for deletions).
    pub hunks: Vec<Vec<Option<Numbered>>>,
    /// A note instead of (or beside) hunks: new, deleted, binary.
    pub note: Option<String>,
}

/// Parse `git diff` output into files.
pub fn parse_unified(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = vec![];
    let (mut o, mut n) = (0usize, 0usize);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, p)| p.to_string())
                .unwrap_or_else(|| rest.to_string());
            files.push(FileDiff {
                path,
                added: 0,
                removed: 0,
                hunks: vec![],
                note: None,
            });
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if let Some(rest) = line.strip_prefix("@@ -") {
            let mut parts = rest.split_whitespace();
            let old = parts.next().unwrap_or("1");
            let new = parts.next().unwrap_or("+1").trim_start_matches('+');
            o = old
                .split(',')
                .next()
                .and_then(|x| x.parse().ok())
                .unwrap_or(1);
            n = new
                .split(',')
                .next()
                .and_then(|x| x.parse().ok())
                .unwrap_or(1);
            f.hunks.push(vec![]);
            continue;
        }
        if f.hunks.is_empty() {
            if line.starts_with("Binary files") {
                f.note = Some("binary file".into());
            } else if line.starts_with("new file") {
                f.note = Some("new file".into());
            } else if line.starts_with("deleted file") {
                f.note = Some("deleted".into());
            }
            continue;
        }
        let Some(h) = f.hunks.last_mut() else {
            continue;
        };
        let (tag, text) = match line.chars().next() {
            Some('+') => (ChangeTag::Insert, &line[1..]),
            Some('-') => (ChangeTag::Delete, &line[1..]),
            Some(' ') => (ChangeTag::Equal, &line[1..]),
            _ => continue,
        };
        let num = if tag == ChangeTag::Delete { o } else { n };
        match tag {
            ChangeTag::Delete => {
                o += 1;
                f.removed += 1;
            }
            ChangeTag::Insert => {
                n += 1;
                f.added += 1;
            }
            ChangeTag::Equal => {
                o += 1;
                n += 1;
            }
        }
        h.push(Some((tag, Some(num), text.replace('\t', "    "))));
    }
    files
}
