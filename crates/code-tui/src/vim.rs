//! Vim mode for the composer (D54), adapted from FlyCockpit's
//! Cockpit composer vim dispatch: normal, insert,
//! operator-pending and visual (charwise and linewise) modes with the
//! common motions, operators, text objects and one unnamed register.
//!
//! Every edit goes through [`TextBuf::splice`], so paste chips stay atomic:
//! an operator range touching a chip widens over the whole chip, and the
//! register keeps wire text so a yanked chip is put back as a chip.
//!
//! Esc (Cockpit's rule, D54): in insert mode Esc only switches to normal
//! mode; in normal mode Esc clears a pending `g`/`f`/operator/text object
//! and is otherwise passed on, keeping its app meaning (clear the
//! selection, clear and save input, then cancel a turn on an empty box).
use crate::composer::TextBuf;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Delete,
    Change,
    Yank,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Insert,
    Normal,
    Operator(Op),
    Visual,
    VisualLine,
}

#[derive(Clone, Copy, Debug)]
struct Find {
    target: char,
    till: bool,
    forward: bool,
}

/// What a key did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Consumed (a motion or mode change).
    Handled,
    /// Consumed and the text changed.
    Edited,
    /// Not a vim key here: the app handles it (Enter, arrows in insert
    /// mode, ctrl chords, an idle Esc in normal mode, …).
    Pass,
}

pub struct Vim {
    pub mode: Mode,
    pending_g: bool,
    pending_find: Option<Find>,
    /// After `i`/`a` in operator or visual mode: the object char comes next.
    pending_obj: Option<bool>,
    pending_r: bool,
    last_find: Option<Find>,
    /// The unnamed register: wire text and whether it is linewise.
    register: (String, bool),
    anchor: usize,
}

impl Default for Vim {
    fn default() -> Self {
        Self {
            mode: Mode::Insert,
            pending_g: false,
            pending_find: None,
            pending_obj: None,
            pending_r: false,
            last_find: None,
            register: (String::new(), false),
            anchor: 0,
        }
    }
}

fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

impl Vim {
    /// The status-line label (`-- INSERT --`, `NORMAL`, …).
    pub fn label(&self) -> String {
        match self.mode {
            Mode::Insert => "-- INSERT --".into(),
            Mode::Normal if self.pending() => "NORMAL …".into(),
            Mode::Normal => "NORMAL".into(),
            Mode::Operator(op) => format!(
                "NORMAL {}",
                match op {
                    Op::Delete => "d",
                    Op::Change => "c",
                    Op::Yank => "y",
                }
            ),
            Mode::Visual => "-- VISUAL --".into(),
            Mode::VisualLine => "-- VISUAL LINE --".into(),
        }
    }
    /// A block cursor in every mode but insert.
    pub fn block_cursor(&self) -> bool {
        self.mode != Mode::Insert
    }
    fn pending(&self) -> bool {
        self.pending_g
            || self.pending_find.is_some()
            || self.pending_obj.is_some()
            || self.pending_r
    }
    fn clear_pending(&mut self) {
        self.pending_g = false;
        self.pending_find = None;
        self.pending_obj = None;
        self.pending_r = false;
    }
    /// Back to insert mode with nothing pending (after a send).
    pub fn reset_insert(&mut self) {
        self.clear_pending();
        self.mode = Mode::Insert;
    }

    /// The highlighted range in visual modes.
    pub fn visual_range(&self, buf: &TextBuf) -> Option<(usize, usize)> {
        let (lo, hi) = (
            self.anchor.min(buf.cursor).min(buf.text.len()),
            self.anchor.max(buf.cursor).min(buf.text.len()),
        );
        match self.mode {
            Mode::Visual => Some(buf.widen(lo, buf.next(hi).max(lo))),
            Mode::VisualLine => {
                let end = buf.line_end_of(hi);
                Some((
                    buf.line_start_of(lo),
                    (end + 1).min(buf.text.len()).max(end),
                ))
            }
            _ => None,
        }
    }

    pub fn key(&mut self, buf: &mut TextBuf, key: KeyEvent) -> Outcome {
        let ctrl = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match self.mode {
            Mode::Insert => {
                if key.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                    if buf.cursor > buf.line_start_of(buf.cursor) {
                        buf.cursor = buf.snap(buf.prev(buf.cursor), false);
                    }
                    return Outcome::Handled;
                }
                Outcome::Pass
            }
            _ if ctrl => Outcome::Pass,
            Mode::Normal => self.normal(buf, key),
            Mode::Operator(op) => {
                if key.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                    self.clear_pending();
                    return Outcome::Handled;
                }
                self.operator(buf, op, key)
            }
            Mode::Visual | Mode::VisualLine => self.visual(buf, key),
        }
    }

    // ---- motions ---------------------------------------------------------

    /// Resolve a motion key from `pos`: (target, inclusive, linewise).
    /// None when `c` is not a motion (or a prefix was stored).
    fn motion(&mut self, buf: &TextBuf, c: char) -> Option<(usize, bool, bool)> {
        let pos = buf.cursor;
        let was_g = std::mem::take(&mut self.pending_g);
        if let Some(mut f) = self.pending_find.take() {
            f.target = c;
            self.last_find = Some(f);
            return find(buf, pos, f).map(|p| (p, f.forward, false));
        }
        if was_g {
            return match c {
                'g' => Some((0, false, true)),
                'e' | 'E' => Some((word_end_back(buf, pos, c == 'E'), true, false)),
                _ => None,
            };
        }
        Some(match c {
            'h' => (
                if pos > buf.line_start_of(pos) {
                    buf.prev(pos)
                } else {
                    pos
                },
                false,
                false,
            ),
            'l' | ' ' => (
                if pos < buf.line_end_of(pos) {
                    buf.next(pos)
                } else {
                    pos
                },
                false,
                false,
            ),
            'j' => (buf.vertical_from(pos, false)?, false, true),
            'k' => (buf.vertical_from(pos, true)?, false, true),
            'w' | 'W' => (word_forward(buf, pos, c == 'W'), false, false),
            'b' | 'B' => (word_backward(buf, pos, c == 'B'), false, false),
            'e' | 'E' => (word_end(buf, pos, c == 'E'), true, false),
            '0' => (buf.line_start_of(pos), false, false),
            '^' => (first_non_blank(buf, pos), false, false),
            '$' => (buf.line_end_of(pos), false, false),
            'G' => (buf.line_start_of(buf.text.len()), false, true),
            '%' => (match_bracket(buf, pos)?, true, false),
            ';' | ',' => {
                let mut f = self.last_find?;
                if c == ',' {
                    f.forward = !f.forward;
                }
                // `;` after `t` steps over the adjacent target.
                let from = if f.till {
                    if f.forward {
                        buf.next(pos)
                    } else {
                        buf.prev(pos)
                    }
                } else {
                    pos
                };
                (find(buf, from, f).filter(|&p| p != pos)?, f.forward, false)
            }
            'g' => {
                self.pending_g = true;
                return None;
            }
            'f' | 'F' | 't' | 'T' => {
                self.pending_find = Some(Find {
                    target: '\0',
                    till: matches!(c, 't' | 'T'),
                    forward: matches!(c, 'f' | 't'),
                });
                return None;
            }
            _ => return None,
        })
    }

    fn is_prefix(&self) -> bool {
        self.pending_g || self.pending_find.is_some()
    }

    // ---- normal ----------------------------------------------------------

    fn normal(&mut self, buf: &mut TextBuf, key: KeyEvent) -> Outcome {
        let c = match key.code {
            KeyCode::Esc => {
                if self.pending() {
                    self.clear_pending();
                    return Outcome::Handled;
                }
                return Outcome::Pass;
            }
            KeyCode::Char(c) => c,
            KeyCode::Left => 'h',
            KeyCode::Right => 'l',
            KeyCode::Backspace => 'h',
            KeyCode::Delete => 'x',
            KeyCode::Home => '0',
            KeyCode::End => '$',
            // Enter sends, Up/Down recall history, Tab completes.
            _ => return Outcome::Pass,
        };
        if self.pending_r {
            self.pending_r = false;
            if buf.cursor < buf.line_end_of(buf.cursor) {
                let n = buf.next(buf.cursor);
                let (a, _) = buf.splice(buf.cursor, n, c.encode_utf8(&mut [0; 4]));
                buf.cursor = a;
                return Outcome::Edited;
            }
            return Outcome::Handled;
        }
        let prefixed = self.is_prefix();
        if let Some((target, _, _)) = self.motion(buf, c) {
            buf.cursor = buf.snap(target, target > buf.cursor);
            self.clamp(buf);
            return Outcome::Handled;
        }
        if prefixed || self.is_prefix() {
            return Outcome::Handled;
        }
        let edited = match c {
            'i' => {
                self.mode = Mode::Insert;
                false
            }
            'I' => {
                buf.cursor = first_non_blank(buf, buf.cursor);
                self.mode = Mode::Insert;
                false
            }
            'a' => {
                if buf.cursor < buf.line_end_of(buf.cursor) {
                    buf.cursor = buf.snap(buf.next(buf.cursor), true);
                }
                self.mode = Mode::Insert;
                false
            }
            'A' => {
                buf.cursor = buf.line_end_of(buf.cursor);
                self.mode = Mode::Insert;
                false
            }
            'o' => {
                buf.cursor = buf.line_end_of(buf.cursor);
                buf.insert("\n");
                self.mode = Mode::Insert;
                true
            }
            'O' => {
                buf.cursor = buf.line_start_of(buf.cursor);
                buf.insert("\n");
                buf.cursor -= 1;
                self.mode = Mode::Insert;
                true
            }
            'v' => {
                self.anchor = buf.cursor;
                self.mode = Mode::Visual;
                false
            }
            'V' => {
                self.anchor = buf.cursor;
                self.mode = Mode::VisualLine;
                false
            }
            'x' | 's' => {
                let at = buf.cursor;
                if at < buf.line_end_of(at) {
                    self.apply(buf, Op::Delete, at, buf.next(at), false);
                }
                if c == 's' {
                    self.mode = Mode::Insert;
                }
                true
            }
            'X' => {
                let at = buf.cursor;
                if at > buf.line_start_of(at) {
                    self.apply(buf, Op::Delete, buf.prev(at), at, false);
                }
                true
            }
            'D' | 'C' => {
                let (a, b) = (buf.cursor, buf.line_end_of(buf.cursor));
                self.apply(
                    buf,
                    if c == 'C' { Op::Change } else { Op::Delete },
                    a,
                    b,
                    false,
                );
                true
            }
            'S' => {
                let p = buf.cursor;
                self.apply(buf, Op::Change, p, p, true);
                true
            }
            'J' => {
                let end = buf.line_end_of(buf.cursor);
                if end < buf.text.len() {
                    let rest = &buf.text[end + 1..];
                    let ws = rest.len() - rest.trim_start_matches([' ', '\t']).len();
                    let (a, _) = buf.splice(end, end + 1 + ws, " ");
                    buf.cursor = a;
                }
                true
            }
            'r' => {
                self.pending_r = true;
                false
            }
            'p' => {
                self.put(buf, true);
                true
            }
            'P' => {
                self.put(buf, false);
                true
            }
            'd' => {
                self.mode = Mode::Operator(Op::Delete);
                false
            }
            'c' => {
                self.mode = Mode::Operator(Op::Change);
                false
            }
            'y' => {
                self.mode = Mode::Operator(Op::Yank);
                false
            }
            _ => return Outcome::Handled,
        };
        if self.mode == Mode::Normal {
            self.clamp(buf);
        }
        if edited {
            Outcome::Edited
        } else {
            Outcome::Handled
        }
    }

    /// Normal-mode cursors sit on a character, not past the line end.
    fn clamp(&self, buf: &mut TextBuf) {
        if self.mode == Mode::Insert {
            return;
        }
        let p = buf.cursor.min(buf.text.len());
        let start = buf.line_start_of(p);
        if p > start && (p == buf.text.len() || buf.text[p..].starts_with('\n')) {
            buf.cursor = buf.snap(buf.prev(p), false);
        } else {
            buf.cursor = buf.snap(p, false);
        }
    }

    // ---- operators -------------------------------------------------------

    fn operator(&mut self, buf: &mut TextBuf, op: Op, key: KeyEvent) -> Outcome {
        let c = match key.code {
            KeyCode::Char(c) => c,
            KeyCode::Left => 'h',
            KeyCode::Right => 'l',
            KeyCode::Up => 'k',
            KeyCode::Down => 'j',
            _ => {
                self.mode = Mode::Normal;
                self.clear_pending();
                return Outcome::Handled;
            }
        };
        if let Some(around) = self.pending_obj.take() {
            self.mode = Mode::Normal;
            if let Some((a, b)) = text_object(buf, c, around) {
                self.apply(buf, op, a, b, false);
                return Outcome::Edited;
            }
            return Outcome::Handled;
        }
        let doubled = matches!(
            (op, c),
            (Op::Delete, 'd') | (Op::Change, 'c') | (Op::Yank, 'y')
        );
        if doubled && !self.is_prefix() {
            self.mode = Mode::Normal;
            let p = buf.cursor;
            self.apply(buf, op, p, p, true);
            return Outcome::Edited;
        }
        if matches!(c, 'i' | 'a') && !self.is_prefix() {
            self.pending_obj = Some(c == 'a');
            return Outcome::Handled;
        }
        let prefixed = self.is_prefix();
        // `cw` changes to the end of the word, as in vim.
        let c = if op == Op::Change && !prefixed && matches!(c, 'w' | 'W') {
            let on_word = buf.text[buf.cursor..]
                .chars()
                .next()
                .is_some_and(|ch| !ch.is_whitespace());
            if on_word {
                if c == 'w' { 'e' } else { 'E' }
            } else {
                c
            }
        } else {
            c
        };
        match self.motion(buf, c) {
            Some((target, inclusive, linewise)) => {
                self.mode = Mode::Normal;
                let pos = buf.cursor;
                let (mut a, mut b) = (pos.min(target), pos.max(target));
                if inclusive && b < buf.text.len() {
                    b = buf.next(b);
                }
                if linewise {
                    a = a.min(pos);
                }
                self.apply(buf, op, a, b, linewise);
                Outcome::Edited
            }
            None if prefixed || self.is_prefix() => Outcome::Handled,
            None => {
                self.mode = Mode::Normal;
                Outcome::Handled
            }
        }
    }

    /// Apply `op` to `a..b` (whole lines when `linewise`), chips widened.
    fn apply(&mut self, buf: &mut TextBuf, op: Op, a: usize, b: usize, linewise: bool) {
        let (mut a, mut b) = buf.widen(a, b);
        if linewise {
            a = buf.line_start_of(a);
            b = buf.line_end_of(b);
            let keep_line = op == Op::Change;
            let text = format!("{}\n", buf.wire_range(a, b));
            self.register = (text, true);
            match op {
                Op::Yank => {
                    buf.cursor = a;
                }
                _ if keep_line => {
                    let (s, _) = buf.splice(a, b, "");
                    buf.cursor = s;
                    self.mode = Mode::Insert;
                }
                _ => {
                    // Take the line break too: the next one, else the previous.
                    if b < buf.text.len() {
                        b += 1;
                    } else {
                        a = a.saturating_sub(1);
                    }
                    let (s, _) = buf.splice(a, b, "");
                    let s = s.min(buf.text.len());
                    buf.cursor = first_non_blank(buf, s);
                }
            }
        } else {
            self.register = (buf.wire_range(a, b), false);
            match op {
                Op::Yank => buf.cursor = a,
                Op::Delete => buf.cursor = buf.splice(a, b, "").0,
                Op::Change => {
                    buf.cursor = buf.splice(a, b, "").0;
                    self.mode = Mode::Insert;
                }
            }
        }
        if self.mode == Mode::Normal {
            self.clamp(buf);
        }
    }

    /// `p` / `P`.
    fn put(&mut self, buf: &mut TextBuf, after: bool) {
        let (text, linewise) = self.register.clone();
        if text.is_empty() {
            return;
        }
        if linewise {
            let body = text.strip_suffix('\n').unwrap_or(&text);
            if after {
                let end = buf.line_end_of(buf.cursor);
                buf.cursor = end;
                buf.insert("\n");
                let start = buf.cursor;
                buf.insert_wire(body);
                buf.cursor = start;
            } else {
                let start = buf.line_start_of(buf.cursor);
                buf.cursor = start;
                buf.insert_wire(body);
                buf.insert("\n");
                buf.cursor = start;
            }
        } else {
            if after && buf.cursor < buf.line_end_of(buf.cursor) {
                buf.cursor = buf.snap(buf.next(buf.cursor), true);
            }
            buf.insert_wire(&text);
            if buf.cursor > 0 {
                buf.cursor = buf.snap(buf.prev(buf.cursor), false);
            }
        }
    }

    // ---- visual ----------------------------------------------------------

    fn visual(&mut self, buf: &mut TextBuf, key: KeyEvent) -> Outcome {
        let c = match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.clear_pending();
                self.clamp(buf);
                return Outcome::Handled;
            }
            KeyCode::Char(c) => c,
            KeyCode::Left => 'h',
            KeyCode::Right => 'l',
            KeyCode::Up => 'k',
            KeyCode::Down => 'j',
            KeyCode::Home => '0',
            KeyCode::End => '$',
            _ => return Outcome::Handled,
        };
        if let Some(around) = self.pending_obj.take() {
            if let Some((a, b)) = text_object(buf, c, around) {
                self.anchor = a;
                buf.cursor = buf.prev(b).max(a);
                self.mode = Mode::Visual;
            }
            return Outcome::Handled;
        }
        let prefixed = self.is_prefix();
        if let Some((target, _, _)) = self.motion(buf, c) {
            buf.cursor = buf.snap(target, target > buf.cursor);
            return Outcome::Handled;
        }
        if prefixed || self.is_prefix() {
            return Outcome::Handled;
        }
        let linewise = self.mode == Mode::VisualLine;
        let range = self.visual_range(buf);
        let op = match c {
            'd' | 'x' | 'D' | 'X' => Some(Op::Delete),
            'c' | 's' | 'C' | 'S' => Some(Op::Change),
            'y' | 'Y' => Some(Op::Yank),
            'o' => {
                std::mem::swap(&mut self.anchor, &mut buf.cursor);
                return Outcome::Handled;
            }
            'v' => {
                self.mode = if linewise { Mode::Visual } else { Mode::Normal };
                return Outcome::Handled;
            }
            'V' => {
                self.mode = if linewise {
                    Mode::Normal
                } else {
                    Mode::VisualLine
                };
                return Outcome::Handled;
            }
            'i' | 'a' => {
                self.pending_obj = Some(c == 'a');
                return Outcome::Handled;
            }
            'p' | 'P' => {
                // Replace the selection with the register.
                let saved = self.register.clone();
                if let Some((a, b)) = range {
                    self.mode = Mode::Normal;
                    let (s, _) = buf.splice(a, b, "");
                    buf.cursor = s;
                    self.register = saved;
                    let lw = self.register.1;
                    self.register.1 = false;
                    if lw {
                        let body = self.register.0.trim_end_matches('\n').to_string();
                        buf.insert_wire(&body);
                    } else {
                        let text = self.register.0.clone();
                        buf.insert_wire(&text);
                    }
                    self.register.1 = lw;
                    self.clamp(buf);
                    return Outcome::Edited;
                }
                return Outcome::Handled;
            }
            _ => None,
        };
        let (Some(op), Some((a, b))) = (op, range) else {
            return Outcome::Handled;
        };
        self.mode = Mode::Normal;
        if linewise {
            let last = if b > a { buf.prev(b) } else { a };
            self.apply(buf, op, a, last, true);
        } else {
            self.apply(buf, op, a, b, false);
        }
        if op == Op::Yank {
            Outcome::Handled
        } else {
            Outcome::Edited
        }
    }
}

// ---- motion helpers ------------------------------------------------------

fn chars(buf: &TextBuf) -> Vec<(usize, char)> {
    buf.text.char_indices().collect()
}

fn index_of(cs: &[(usize, char)], pos: usize) -> usize {
    cs.partition_point(|&(b, _)| b < pos)
}

fn byte_at(buf: &TextBuf, cs: &[(usize, char)], i: usize) -> usize {
    cs.get(i).map_or(buf.text.len(), |&(b, _)| b)
}

fn word_forward(buf: &TextBuf, pos: usize, big: bool) -> usize {
    let cs = chars(buf);
    let mut i = index_of(&cs, pos);
    if i >= cs.len() {
        return buf.text.len();
    }
    let k = class(cs[i].1, big);
    while i < cs.len() && k != 0 && class(cs[i].1, big) == k {
        i += 1;
    }
    while i < cs.len() && class(cs[i].1, big) == 0 {
        i += 1;
    }
    byte_at(buf, &cs, i)
}

fn word_backward(buf: &TextBuf, pos: usize, big: bool) -> usize {
    let cs = chars(buf);
    let mut i = index_of(&cs, pos);
    while i > 0 && class(cs[i - 1].1, big) == 0 {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let k = class(cs[i - 1].1, big);
    while i > 0 && class(cs[i - 1].1, big) == k {
        i -= 1;
    }
    byte_at(buf, &cs, i)
}

fn word_end(buf: &TextBuf, pos: usize, big: bool) -> usize {
    let cs = chars(buf);
    let mut i = index_of(&cs, pos) + 1;
    while i < cs.len() && class(cs[i].1, big) == 0 {
        i += 1;
    }
    if i >= cs.len() {
        return byte_at(buf, &cs, cs.len().saturating_sub(1));
    }
    let k = class(cs[i].1, big);
    while i + 1 < cs.len() && class(cs[i + 1].1, big) == k {
        i += 1;
    }
    byte_at(buf, &cs, i)
}

fn word_end_back(buf: &TextBuf, pos: usize, big: bool) -> usize {
    let cs = chars(buf);
    let mut i = index_of(&cs, pos);
    if i >= cs.len() {
        i = cs.len();
    }
    if i < cs.len() {
        let k = class(cs[i].1, big);
        while i > 0 && k != 0 && class(cs[i].1, big) == k {
            i -= 1;
        }
    }
    while i > 0 && class(cs[i.min(cs.len() - 1)].1, big) == 0 {
        i -= 1;
    }
    byte_at(buf, &cs, i)
}

fn first_non_blank(buf: &TextBuf, pos: usize) -> usize {
    let start = buf.line_start_of(pos);
    let line = &buf.text[start..buf.line_end_of(pos)];
    start + (line.len() - line.trim_start_matches([' ', '\t']).len())
}

fn find(buf: &TextBuf, pos: usize, f: Find) -> Option<usize> {
    let (start, end) = (buf.line_start_of(pos), buf.line_end_of(pos));
    if f.forward {
        let from = buf.next(pos).min(end);
        let rel = buf.text[from..end].find(f.target)?;
        let hit = from + rel;
        Some(if f.till { buf.prev(hit).max(pos) } else { hit })
    } else {
        let hit = buf.text[start..pos].rfind(f.target)? + start;
        Some(if f.till { buf.next(hit).min(pos) } else { hit })
    }
}

fn match_bracket(buf: &TextBuf, pos: usize) -> Option<usize> {
    let pairs = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')];
    let cs = chars(buf);
    let mut i = index_of(&cs, pos);
    // From the cursor, the first bracket on the line.
    while i < cs.len()
        && cs[i].1 != '\n'
        && !pairs.iter().any(|&(o, c)| cs[i].1 == o || cs[i].1 == c)
    {
        i += 1;
    }
    let ch = cs.get(i)?.1;
    let (open, close, forward) = pairs.iter().find_map(|&(o, c)| {
        (ch == o)
            .then_some((o, c, true))
            .or((ch == c).then_some((o, c, false)))
    })?;
    let mut depth = 0i32;
    if forward {
        for &(b, c) in &cs[i..] {
            depth += (c == open) as i32 - (c == close) as i32;
            if depth == 0 {
                return Some(b);
            }
        }
    } else {
        for &(b, c) in cs[..=i].iter().rev() {
            depth += (c == close) as i32 - (c == open) as i32;
            if depth == 0 {
                return Some(b);
            }
        }
    }
    None
}

/// `iw` `aw` `iW` `aW` `i"` `a"` `i'` `a'` `` i` `` `i(` `a(` `ib` `i[` `i{` `iB` `i<`.
fn text_object(buf: &TextBuf, obj: char, around: bool) -> Option<(usize, usize)> {
    let pos = buf.cursor;
    match obj {
        'w' | 'W' => {
            let cs = chars(buf);
            let i = index_of(&cs, pos);
            if i >= cs.len() {
                return None;
            }
            let big = obj == 'W';
            let k = class(cs[i].1, big);
            let (mut a, mut b) = (i, i);
            while a > 0 && class(cs[a - 1].1, big) == k && cs[a - 1].1 != '\n' {
                a -= 1;
            }
            while b + 1 < cs.len() && class(cs[b + 1].1, big) == k && cs[b + 1].1 != '\n' {
                b += 1;
            }
            let mut end = b + 1;
            if around {
                let trailing = end;
                while end < cs.len() && cs[end].1 != '\n' && class(cs[end].1, big) == 0 {
                    end += 1;
                }
                if end == trailing {
                    while a > 0 && cs[a - 1].1 != '\n' && class(cs[a - 1].1, big) == 0 {
                        a -= 1;
                    }
                }
            }
            Some((byte_at(buf, &cs, a), byte_at(buf, &cs, end)))
        }
        '"' | '\'' | '`' => {
            let (start, end) = (buf.line_start_of(pos), buf.line_end_of(pos));
            let line = &buf.text[start..end];
            let quotes: Vec<usize> = line
                .char_indices()
                .filter(|&(_, c)| c == obj)
                .map(|(i, _)| start + i)
                .collect();
            let pair = quotes
                .chunks(2)
                .filter(|p| p.len() == 2)
                .find(|p| p[0] <= pos && pos <= p[1])
                .or_else(|| quotes.chunks(2).find(|p| p.len() == 2 && p[0] > pos))?;
            let (q0, q1) = (pair[0], pair[1]);
            Some(if around {
                (q0, q1 + obj.len_utf8())
            } else {
                (q0 + obj.len_utf8(), q1)
            })
        }
        '(' | ')' | 'b' | '[' | ']' | '{' | '}' | 'B' | '<' | '>' => {
            let (open, close) = match obj {
                '(' | ')' | 'b' => ('(', ')'),
                '[' | ']' => ('[', ']'),
                '{' | '}' | 'B' => ('{', '}'),
                _ => ('<', '>'),
            };
            let cs = chars(buf);
            let i = index_of(&cs, pos).min(cs.len());
            let mut depth = 0i32;
            let mut o = None;
            let mut j = i.min(cs.len().saturating_sub(1)) as isize;
            if cs.get(i).is_some_and(|&(_, c)| c == open) {
                o = Some(i);
            } else {
                while j >= 0 {
                    let c = cs[j as usize].1;
                    if c == close && (j as usize) != i {
                        depth += 1;
                    } else if c == open {
                        if depth == 0 {
                            o = Some(j as usize);
                            break;
                        }
                        depth -= 1;
                    }
                    j -= 1;
                }
            }
            let o = o?;
            let mut depth = 0i32;
            let mut cl = None;
            for (k, &(_, c)) in cs.iter().enumerate().skip(o) {
                depth += (c == open) as i32 - (c == close) as i32;
                if depth == 0 {
                    cl = Some(k);
                    break;
                }
            }
            let cl = cl?;
            Some(if around {
                (byte_at(buf, &cs, o), byte_at(buf, &cs, cl + 1))
            } else {
                (byte_at(buf, &cs, o + 1), byte_at(buf, &cs, cl))
            })
        }
        _ => None,
    }
}
