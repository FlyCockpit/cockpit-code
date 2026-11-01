//! Multi-line composer with history recall, paste blocks (D53) and an
//! optional vim mode (D54), plus the single-line input used by dialogs and
//! the model filter. Positions are byte offsets on char boundaries; layout
//! hard-wraps by display width.
//!
//! Paste blocks follow Cockpit's span registry: the chip label is ordinary
//! buffer text and [`TextBuf::blocks`] tracks its byte range. Every edit
//! goes through [`TextBuf::splice`], which widens an edit touching a chip to
//! the whole chip and shifts the ranges after it, so a chip is always
//! inserted, deleted and skipped as one unit.
use crate::paste::{self, Block};
use crate::theme::theme;
use crate::vim::Vim;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct TextBuf {
    pub text: String,
    pub cursor: usize,
    /// Paste chips, sorted by position, never overlapping.
    pub blocks: Vec<Block>,
    /// Last paste number handed out.
    last_n: u32,
    /// Last attachment number handed out.
    last_a: u32,
}

/// A laid-out buffer: rows, the cursor (column, row) and the chips' cells
/// (row, first column, end column, block index).
pub struct BufLayout {
    pub rows: Vec<Line<'static>>,
    pub cursor: (u16, u16),
    pub chips: Vec<(usize, u16, u16, usize)>,
}

impl TextBuf {
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.blocks.clear();
        self.last_n = 0;
        self.last_a = 0;
    }
    pub fn clear(&mut self) {
        self.set(String::new());
    }

    // ---- the one mutation point ------------------------------------------

    /// Widen `start..end` over every chip it touches.
    pub fn widen(&self, mut start: usize, mut end: usize) -> (usize, usize) {
        loop {
            let mut changed = false;
            for b in &self.blocks {
                let hit = if start == end {
                    b.start < start && start < b.end
                } else {
                    b.start < end && b.end > start
                };
                if hit && (b.start < start || b.end > end) {
                    start = start.min(b.start);
                    end = end.max(b.end);
                    changed = true;
                }
            }
            if !changed {
                return (start, end);
            }
        }
    }

    /// Replace `start..end` with `ins`, atomically over chips; returns the
    /// (start, end) of the inserted text.
    pub fn splice(&mut self, start: usize, end: usize, ins: &str) -> (usize, usize) {
        let (mut start, mut end) = (start.min(self.text.len()), end.min(self.text.len()));
        if start == end {
            // An insertion inside a chip lands after it.
            if let Some(b) = self
                .blocks
                .iter()
                .find(|b| b.start < start && start < b.end)
            {
                start = b.end;
                end = b.end;
            }
        } else {
            (start, end) = self.widen(start, end);
        }
        self.blocks
            .retain(|b| !(start < end && b.start < end && b.end > start));
        self.text.replace_range(start..end, ins);
        let delta = ins.len() as isize - (end - start) as isize;
        for b in &mut self.blocks {
            if b.start >= end {
                b.start = (b.start as isize + delta) as usize;
                b.end = (b.end as isize + delta) as usize;
            }
        }
        (start, start + ins.len())
    }

    /// Move `pos` out of a chip: to its end when moving forward.
    pub fn snap(&self, pos: usize, forward: bool) -> usize {
        match self.blocks.iter().find(|b| b.start < pos && pos < b.end) {
            Some(b) if forward => b.end,
            Some(b) => b.start,
            None => pos,
        }
    }

    // ---- editing ---------------------------------------------------------

    pub fn insert(&mut self, s: &str) {
        self.cursor = self.splice(self.cursor, self.cursor, s).1;
    }
    /// Insert `full` as a paste chip at the cursor.
    pub fn insert_block(&mut self, full: &str) {
        self.last_n += 1;
        let n = self.last_n;
        let label = paste::label(n, full);
        let (a, b) = self.splice(self.cursor, self.cursor, &label);
        let block = Block {
            start: a,
            end: b,
            full: full.to_string(),
            id: paste::fresh_id(full),
            n,
            attach: None,
            warn: false,
        };
        let at = self.blocks.partition_point(|x| x.start < a);
        self.blocks.insert(at, block);
        self.cursor = b;
    }
    /// Insert an attachment chip (D79) for `path` at the cursor.
    pub fn insert_attachment(&mut self, kind: &str, path: &str) {
        self.last_a += 1;
        let n = self.last_a;
        let label = crate::attach::label(n, kind, path);
        let (a, b) = self.splice(self.cursor, self.cursor, &label);
        let block = Block {
            start: a,
            end: b,
            full: path.to_string(),
            id: paste::fresh_id(path),
            n,
            attach: Some(kind.to_string()),
            warn: false,
        };
        let at = self.blocks.partition_point(|x| x.start < a);
        self.blocks.insert(at, block);
        self.cursor = b;
    }
    /// Replace `start..end` (an `@path` mention) with an attachment chip;
    /// the cursor keeps its place relative to the text after it.
    pub fn attach_range(&mut self, start: usize, end: usize, path: &str) {
        let kind = crate::attach::kind_of(std::path::Path::new(path));
        let after = self.cursor >= end;
        let cursor = self.cursor;
        self.cursor = start;
        self.splice(start, end, "");
        self.insert_attachment(kind, path);
        let b = self.cursor;
        self.cursor = if after {
            cursor - (end - start) + (b - start)
        } else {
            cursor.min(start)
        };
    }
    /// Insert a wire message, turning its paste tags back into chips.
    pub fn insert_wire(&mut self, text: &str) {
        for seg in paste::parse(text) {
            match seg {
                paste::Seg::Text(t) => self.insert(t),
                paste::Seg::Paste { body, .. } => self.insert_block(body),
                paste::Seg::Attach { kind, path, .. } => self.insert_attachment(kind, path),
            }
        }
    }
    pub fn set_wire(&mut self, text: &str) {
        self.clear();
        self.insert_wire(text);
    }
    /// The text with chips expanded into tagged pastes (what is sent).
    pub fn to_wire(&self) -> String {
        self.wire_range(0, self.text.len())
    }
    /// `start..end` as wire text (the range must not split a chip).
    pub fn wire_range(&self, start: usize, end: usize) -> String {
        let mut out = String::new();
        let mut at = start;
        for b in self
            .blocks
            .iter()
            .filter(|b| b.start >= start && b.end <= end)
        {
            out.push_str(&self.text[at..b.start]);
            out.push_str(&b.wire());
            at = b.end;
        }
        out.push_str(&self.text[at..end]);
        out
    }
    /// Replace chip `i`'s text (the label is renumbered in place).
    pub fn set_block_text(&mut self, i: usize, full: String) {
        let Some(b) = self.blocks.get(i).cloned().filter(|b| b.attach.is_none()) else {
            return;
        };
        let label = paste::label(b.n, &full);
        let cursor_after = self.cursor >= b.end;
        self.blocks.remove(i);
        let (a, e) = self.splice(b.start, b.end, &label);
        let id = if full == b.full {
            b.id
        } else {
            paste::fresh_id(&full)
        };
        self.blocks.insert(
            i,
            Block {
                start: a,
                end: e,
                full,
                id,
                n: b.n,
                attach: None,
                warn: false,
            },
        );
        if cursor_after {
            self.cursor = (self.cursor as isize + (e - a) as isize - (b.end - b.start) as isize)
                .max(0) as usize;
        }
    }
    /// Replace chip `i` with its text inline.
    pub fn expand_block(&mut self, i: usize) {
        let Some(b) = self.blocks.get(i).cloned() else {
            return;
        };
        self.blocks.remove(i);
        let inline = match &b.attach {
            Some(_) => crate::attach::mention(&b.full),
            None => b.full.clone(),
        };
        let (_, e) = self.splice(b.start, b.end, &inline);
        self.cursor = e;
    }
    pub fn remove_block(&mut self, i: usize) {
        let Some(b) = self.blocks.get(i).cloned() else {
            return;
        };
        let (a, _) = self.splice(b.start, b.end, "");
        self.cursor = a;
    }

    /// The buffer for `$EDITOR`: chips expanded between Cockpit's
    /// `--- paste #n start ---` / `--- paste #n end ---` delimiters
    /// (`append_display_paste`), nothing else changed.
    pub fn editor_text(&self) -> String {
        let mut out = String::new();
        let mut at = 0;
        for b in &self.blocks {
            out.push_str(&self.text[at..b.start]);
            // Attachments edit as `@path`; they become chips again on send.
            if b.attach.is_some() {
                out.push_str(&crate::attach::mention(&b.full));
                at = b.end;
                continue;
            }
            out.push_str(&paste::editor_open(b.n));
            out.push('\n');
            out.push_str(&b.full);
            if !b.full.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&paste::editor_close(b.n));
            at = b.end;
        }
        out.push_str(&self.text[at..]);
        out
    }
    /// Rebuild from edited `$EDITOR` text. Intact delimiters of a known
    /// paste become its chip again (the id kept when its text is
    /// unchanged; an edited text stays a chip with the new text, as in
    /// Cockpit); a removed or damaged delimiter leaves plain text.
    pub fn rebuild_from_editor(&mut self, text: &str) {
        let mut old = std::mem::take(&mut self.blocks);
        old.retain(|b| b.attach.is_none());
        self.text.clear();
        self.cursor = 0;
        let find = |rest: &str, n: u32| -> Option<(usize, usize, usize, usize)> {
            let open = format!("{}\n", paste::editor_open(n));
            let at = rest.find(&open)?;
            let b0 = at + open.len();
            let close = format!("\n{}", paste::editor_close(n));
            // An empty body: the open line directly followed by the close line.
            let e = if rest[b0..].starts_with(&paste::editor_close(n)) {
                return Some((at, b0, b0, b0 + paste::editor_close(n).len()));
            } else {
                rest[b0..].find(&close)?
            };
            Some((at, b0, b0 + e, b0 + e + close.len()))
        };
        let mut rest = text;
        loop {
            let next = old
                .iter()
                .filter_map(|b| find(rest, b.n).map(|m| (m, b)))
                .min_by_key(|(m, _)| m.0);
            let Some(((at, b0, b1, end), b)) = next else {
                self.insert(rest);
                break;
            };
            self.insert(&rest[..at]);
            let mut body = rest[b0..b1].to_string();
            if b.full.ends_with('\n') {
                body.push('\n');
            }
            let label = paste::label(b.n, &body);
            let (s, e) = self.splice(self.cursor, self.cursor, &label);
            let id = if body == b.full {
                b.id.clone()
            } else {
                paste::fresh_id(&body)
            };
            let i = self.blocks.partition_point(|x| x.start < s);
            self.blocks.insert(
                i,
                Block {
                    start: s,
                    end: e,
                    full: body,
                    id,
                    n: b.n,
                    attach: None,
                    warn: false,
                },
            );
            self.cursor = e;
            rest = &rest[end..];
        }
        self.cursor = self.text.len();
    }

    pub(crate) fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().last().map_or(0, |(p, _)| p)
    }
    pub(crate) fn next(&self, i: usize) -> usize {
        self.text[i..]
            .chars()
            .next()
            .map_or(i, |c| i + c.len_utf8())
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let p = self.prev(self.cursor);
            self.cursor = self.splice(p, self.cursor, "").0;
        }
    }
    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let n = self.next(self.cursor);
            self.cursor = self.splice(self.cursor, n, "").0;
        }
    }
    pub fn left(&mut self) {
        self.cursor = self.snap(self.prev(self.cursor), false);
    }
    pub fn right(&mut self) {
        self.cursor = self.snap(self.next(self.cursor), true);
    }
    pub fn word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        let p = trimmed.rfind(|c: char| c.is_whitespace()).map_or(0, |p| {
            p + trimmed[p..].chars().next().map_or(1, char::len_utf8)
        });
        self.cursor = self.snap(p, false);
    }
    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skip = after.len() - after.trim_start().len();
        let rest = &after[skip..];
        let word = rest.find(char::is_whitespace).unwrap_or(rest.len());
        self.cursor = self.snap(self.cursor + skip + word, true);
    }
    pub fn delete_word(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.cursor = self.splice(self.cursor, end, "").0;
    }
    pub(crate) fn line_start_of(&self, pos: usize) -> usize {
        self.text[..pos].rfind('\n').map_or(0, |p| p + 1)
    }
    pub(crate) fn line_end_of(&self, pos: usize) -> usize {
        self.text[pos..]
            .find('\n')
            .map_or(self.text.len(), |p| pos + p)
    }
    pub fn home(&mut self) {
        self.cursor = self.line_start_of(self.cursor);
    }
    pub fn end(&mut self) {
        self.cursor = self.line_end_of(self.cursor);
    }
    pub fn kill_line(&mut self) {
        let start = self.line_start_of(self.cursor);
        self.cursor = self.splice(start, self.cursor, "").0;
    }
    /// The position on the previous/next logical line at the same char
    /// column (None on the first/last line).
    pub(crate) fn vertical_pos(&self, up: bool) -> Option<usize> {
        self.vertical_from(self.cursor, up)
    }
    pub(crate) fn vertical_from(&self, pos: usize, up: bool) -> Option<usize> {
        let start = self.line_start_of(pos);
        let col = self.text[start..pos].chars().count();
        let target_start = if up {
            if start == 0 {
                return None;
            }
            self.text[..start - 1].rfind('\n').map_or(0, |p| p + 1)
        } else {
            let end = self.line_end_of(pos);
            if end == self.text.len() {
                return None;
            }
            end + 1
        };
        let line_end = self.line_end_of(target_start);
        let pos = self.text[target_start..line_end]
            .char_indices()
            .nth(col)
            .map_or(line_end, |(i, _)| target_start + i);
        Some(self.snap(pos, false))
    }
    /// Move to the previous/next logical line at the same char column.
    /// Returns false when already on the first/last line.
    pub fn vertical(&mut self, up: bool) -> bool {
        match self.vertical_pos(up) {
            Some(p) => {
                self.cursor = p;
                true
            }
            None => false,
        }
    }

    /// Lay out with `prefix` on the first row and `cont` on the others,
    /// returning the rows and the cursor's (column, row).
    pub fn layout(
        &self,
        width: usize,
        prefix: Span<'static>,
        cont: Span<'static>,
        style: Style,
    ) -> (Vec<Line<'static>>, (u16, u16)) {
        let l = self.layout_ex(width, prefix, cont, style, None);
        (l.rows, l.cursor)
    }

    /// Where rows wrap (byte offsets that start a new row): after the
    /// last space that fits, like the transcript; a word longer than the
    /// row breaks inside it; a chip moves to the next row when it fits there.
    fn soft_breaks(&self, avail: usize) -> std::collections::HashSet<usize> {
        let mut breaks = std::collections::HashSet::new();
        let mut col = 0usize;
        let mut space: Option<(usize, usize)> = None;
        let in_chip = |i: usize| self.blocks.iter().any(|b| b.start <= i && i < b.end);
        for (i, ch) in self.text.char_indices() {
            if let Some(b) = self.blocks.iter().find(|b| b.start == i) {
                let w: usize = self.text[b.start..b.end]
                    .chars()
                    .map(|c| c.width().unwrap_or(0))
                    .sum();
                if col > 0 && col + w > avail {
                    breaks.insert(i);
                    col = 0;
                    space = None;
                }
            }
            if ch == '\n' {
                col = 0;
                space = None;
                continue;
            }
            let w = ch.width().unwrap_or(0);
            if col + w > avail {
                match space.take() {
                    Some((at, c)) if c > 0 && at <= i => {
                        breaks.insert(at);
                        col -= c;
                    }
                    _ => {
                        breaks.insert(i);
                        col = 0;
                    }
                }
                if col + w > avail && !breaks.contains(&i) {
                    breaks.insert(i);
                    col = 0;
                }
            }
            col += w;
            if ch == ' ' && !in_chip(i) {
                space = Some((i + 1, col));
            }
        }
        breaks
    }

    /// [`Self::layout`] with chips styled and recorded, and an optional
    /// highlighted range (the vim visual selection).
    pub fn layout_ex(
        &self,
        width: usize,
        prefix: Span<'static>,
        cont: Span<'static>,
        style: Style,
        sel: Option<(usize, usize)>,
    ) -> BufLayout {
        let t = theme();
        let chip = Style::default().fg(t.link);
        let pw = prefix.content.chars().count();
        let avail = width.saturating_sub(pw + 1).max(1);
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut cur: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut run_style = style;
        let flush_run = |cur: &mut Vec<Span<'static>>, run: &mut String, st: Style| {
            if !run.is_empty() {
                cur.push(Span::styled(std::mem::take(run), st));
            }
        };
        let push_row = |rows: &mut Vec<Line<'static>>, cur: &mut Vec<Span<'static>>| {
            let p = if rows.is_empty() {
                prefix.clone()
            } else {
                cont.clone()
            };
            let mut spans = vec![p];
            spans.append(cur);
            rows.push(Line::from(spans));
        };
        let breaks = self.soft_breaks(avail);
        let mut col = 0usize;
        let mut cursor = None;
        let mut chips = vec![];
        let mut open_chip: Option<(usize, u16, usize)> = None;
        for (i, ch) in self.text.char_indices() {
            if breaks.contains(&i) {
                flush_run(&mut cur, &mut run, run_style);
                push_row(&mut rows, &mut cur);
                col = 0;
            }
            if let Some((bi, _)) = self.blocks.iter().enumerate().find(|(_, b)| b.start == i) {
                open_chip = Some((rows.len(), (pw + col) as u16, bi));
            }
            if let Some((r, x0, bi)) = open_chip
                && self.blocks[bi].start != i
                && (i >= self.blocks[bi].end)
            {
                chips.push((r, x0, (pw + col) as u16, bi));
                open_chip = None;
            }
            if ch == '\n' {
                if i == self.cursor {
                    cursor = Some((col, rows.len()));
                }
                flush_run(&mut cur, &mut run, run_style);
                push_row(&mut rows, &mut cur);
                col = 0;
                continue;
            }
            let w = ch.width().unwrap_or(0);
            if i == self.cursor {
                cursor = Some((col, rows.len()));
            }
            let in_chip = self.blocks.iter().find(|b| b.start <= i && i < b.end);
            let mut st = match in_chip {
                Some(b) if b.warn => Style::default().fg(t.yellow),
                Some(_) => chip,
                None => style,
            };
            if sel.is_some_and(|(a, b)| a <= i && i < b) {
                st = st.add_modifier(Modifier::REVERSED);
            }
            if st != run_style {
                flush_run(&mut cur, &mut run, run_style);
                run_style = st;
            }
            run.push(ch);
            col += w;
        }
        if let Some((r, x0, bi)) = open_chip {
            chips.push((r, x0, (pw + col) as u16, bi));
        }
        let cursor = cursor.unwrap_or_else(|| {
            if col >= avail {
                flush_run(&mut cur, &mut run, run_style);
                push_row(&mut rows, &mut cur);
                col = 0;
            }
            (col, rows.len())
        });
        flush_run(&mut cur, &mut run, run_style);
        push_row(&mut rows, &mut cur);
        BufLayout {
            rows,
            cursor: ((pw + cursor.0) as u16, cursor.1 as u16),
            chips,
        }
    }
}

/// The main prompt editor: a [`TextBuf`] plus history recall and vim mode.
#[derive(Default)]
pub struct Composer {
    pub buf: TextBuf,
    /// Sent messages as wire text (chips re-collapse on recall).
    history: Vec<String>,
    recall: Option<usize>,
    draft: String,
    /// Esc-cleared wire text, restored before walking sent messages.
    cleared: Option<String>,
    /// Vim state when vim mode is on (D54).
    pub vim: Option<Vim>,
}

impl Composer {
    pub fn is_empty(&self) -> bool {
        self.buf.text.is_empty()
    }
    /// The displayed text (chips as labels).
    pub fn text(&self) -> &str {
        &self.buf.text
    }
    /// The text to send (chips as tagged pastes).
    pub fn wire(&self) -> String {
        self.buf.to_wire()
    }
    /// Fill the composer from wire text (chips re-collapse).
    pub fn set_wire(&mut self, text: &str) {
        self.buf.set_wire(text);
    }
    /// Install this session's root-user history, oldest first.
    pub fn reset_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.buf.clear();
        self.recall = None;
        self.draft.clear();
        self.cleared = None;
        if let Some(v) = &mut self.vim {
            v.reset_insert();
        }
    }
    pub fn remember(&mut self, text: String) {
        if !text.trim().is_empty() {
            self.history.push(text);
        }
    }
    /// Clear without submitting; keep chips and attachments as wire text.
    pub fn clear(&mut self) {
        if !self.is_empty() {
            self.cleared = Some(self.buf.to_wire());
            self.buf.clear();
            self.recall = None;
            self.draft.clear();
        }
    }
    /// Take the message as wire text. Only actual root messages enter history.
    pub fn take(&mut self) -> String {
        let text = self.buf.to_wire();
        self.buf.clear();
        self.recall = None;
        self.draft.clear();
        self.cleared = None;
        if let Some(v) = &mut self.vim {
            v.reset_insert();
        }
        text
    }
    pub fn edited(&mut self) {
        self.recall = None;
        self.cleared = None;
    }
    pub fn history_prev(&mut self) {
        if self.buf.vertical(true) {
            return;
        }
        if self.recall.is_none() {
            if self.is_empty()
                && let Some(cleared) = self.cleared.take()
            {
                self.buf.set_wire(&cleared);
                return;
            }
            if self.history.is_empty() {
                return;
            }
        }
        let idx = match self.recall {
            None => {
                self.draft = self.buf.to_wire();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.recall = Some(idx);
        let text = self.history[idx].clone();
        self.buf.set_wire(&text);
    }
    pub fn history_next(&mut self) {
        if self.buf.vertical(false) {
            return;
        }
        match self.recall {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.recall = Some(i + 1);
                let text = self.history[i + 1].clone();
                self.buf.set_wire(&text);
            }
            Some(_) => {
                self.recall = None;
                let draft = std::mem::take(&mut self.draft);
                self.buf.set_wire(&draft);
            }
        }
    }
    pub fn layout(&self, width: usize, placeholder: &str) -> BufLayout {
        let t = theme();
        let prefix = Span::styled("› ", Style::default().fg(t.accent));
        if self.buf.text.is_empty() {
            return BufLayout {
                rows: vec![Line::from(vec![
                    prefix,
                    Span::styled(placeholder.to_string(), t.faint()),
                ])],
                cursor: (2, 0),
                chips: vec![],
            };
        }
        let sel = self.vim.as_ref().and_then(|v| v.visual_range(&self.buf));
        self.buf
            .layout_ex(width, prefix, Span::raw("  "), t.text(), sel)
    }
}
