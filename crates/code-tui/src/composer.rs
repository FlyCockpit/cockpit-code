//! Multi-line composer with history recall, paste blocks and an
//! optional vim mode, built on [`TextBuf`].
use crate::paste::Block;
use crate::textbuf::{BufLayout, TextBuf};
use crate::theme::theme;
use crate::vim::{Outcome, Vim};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// The main prompt editor: a [`TextBuf`] plus history recall and vim mode.
#[derive(Default)]
pub struct Composer {
    buf: TextBuf,
    /// Sent messages as wire text (chips re-collapse on recall).
    history: Vec<String>,
    recall: Option<usize>,
    draft: String,
    /// Esc-cleared wire text, restored before walking sent messages.
    cleared: Option<String>,
    /// Vim state when vim mode is on.
    vim: Option<Vim>,
}

impl Composer {
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
    /// The displayed text (chips as labels).
    pub fn text(&self) -> &str {
        self.buf.text()
    }
    /// Whether the text is a slash command.
    pub fn is_command(&self) -> bool {
        self.buf.text().trim_start().starts_with('/')
    }
    /// Replace the text (a slash command filled in), cursor at the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.buf.set(text);
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
    /// Drop the text without saving it; back to vim insert mode.
    pub fn discard(&mut self) {
        self.take();
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
        if self.buf.is_empty() {
            return BufLayout {
                rows: vec![Line::from(vec![
                    prefix,
                    Span::styled(placeholder.to_string(), t.faint()),
                ])],
                cursor: (2, 0),
                chips: vec![],
            };
        }
        self.buf
            .layout_ex(width, prefix, Span::raw("  "), t.text(), self.selection())
    }

    // ---- editing ---------------------------------------------------------

    /// Insert typed or pasted text at the cursor.
    pub fn insert(&mut self, text: &str) {
        self.buf.insert(text);
    }
    /// Insert a large paste as one chip at the cursor.
    pub fn insert_paste(&mut self, text: &str) {
        self.buf.insert_block(text);
    }
    /// Insert an attachment chip for `path` at the cursor.
    pub fn attach(&mut self, kind: &str, path: &str) {
        self.buf.insert_attachment(kind, path);
    }
    /// Turn `@path` mentions of existing files into attachment chips (not
    /// in a slash command).
    pub fn attach_mentions(&mut self, cwd: &std::path::Path, complete_only: bool) {
        if !self.is_command() {
            self.buf.attach_mentions(cwd, complete_only);
        }
    }
    /// Replace `start..end` with `text`, cursor after it.
    pub fn replace(&mut self, start: usize, end: usize, text: &str) {
        self.buf.replace(start, end, text);
    }
    /// The text and cursor where an `@` mention may be typed: not in a
    /// slash command, not inside or right after a chip.
    pub fn mention_context(&self) -> Option<(&str, usize)> {
        let cursor = self.buf.cursor();
        (!self.is_command() && !self.buf.touches_chip(cursor)).then(|| (self.buf.text(), cursor))
    }
    /// Move the cursor one logical line up or down.
    pub fn move_vertical(&mut self, up: bool) {
        self.buf.vertical(up);
    }
    /// An editing key: newline, cursor motion, deletion or a typed char.
    /// False when the key is not one of them.
    pub fn edit_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let buf = &mut self.buf;
        match key.code {
            KeyCode::Enter if shift || alt => buf.insert("\n"),
            KeyCode::Char('j') if ctrl => buf.insert("\n"),
            KeyCode::Left if alt || ctrl => buf.word_left(),
            KeyCode::Right if alt || ctrl => buf.word_right(),
            KeyCode::Left => buf.left(),
            KeyCode::Right => buf.right(),
            KeyCode::Home => buf.home(),
            KeyCode::End => buf.end(),
            KeyCode::Char('a') if ctrl => buf.home(),
            KeyCode::Char('u') if ctrl => buf.kill_line(),
            KeyCode::Char('w') if ctrl => buf.delete_word(),
            KeyCode::Backspace if alt || ctrl => buf.delete_word(),
            KeyCode::Backspace => buf.backspace(),
            KeyCode::Delete => buf.delete(),
            KeyCode::Char(c) if !ctrl => buf.insert(c.encode_utf8(&mut [0; 4])),
            _ => return false,
        }
        true
    }

    // ---- chips -----------------------------------------------------------

    /// Paste or attachment chip `i`.
    pub fn chip(&self, i: usize) -> Option<&Block> {
        self.buf.chip(i)
    }
    /// The attachment chips as (kind, path), in order.
    pub fn attachments(&self) -> impl Iterator<Item = (&str, &str)> {
        self.buf.attachments()
    }
    /// Flag the attachment chips `warn` picks by kind (shown in yellow).
    pub fn warn_attachments(&mut self, warn: impl Fn(&str) -> bool) {
        self.buf.warn_attachments(warn);
    }
    /// Replace chip `i` with its text inline.
    pub fn expand_chip(&mut self, i: usize) {
        self.buf.expand_block(i);
    }
    pub fn remove_chip(&mut self, i: usize) {
        self.buf.remove_block(i);
    }
    /// Replace paste chip `i`'s text.
    pub fn set_chip_text(&mut self, i: usize, text: String) {
        self.buf.set_block_text(i, text);
    }
    /// The text for `$EDITOR`, chips expanded between delimiters.
    pub fn editor_text(&self) -> String {
        self.buf.editor_text()
    }
    /// Rebuild from edited `$EDITOR` text.
    pub fn rebuild_from_editor(&mut self, text: &str) {
        self.buf.rebuild_from_editor(text);
    }

    // ---- vim -------------------------------------------------------------

    /// Turn vim mode on (in insert mode).
    pub fn enable_vim(&mut self) {
        self.vim = Some(Vim::default());
    }
    /// Turn vim mode off when on, else on; returns whether it is now on.
    pub fn toggle_vim(&mut self) -> bool {
        if self.vim.take().is_some() {
            return false;
        }
        self.enable_vim();
        true
    }
    pub fn vim_on(&self) -> bool {
        self.vim.is_some()
    }
    /// The vim status-line label when vim mode is on.
    pub fn vim_label(&self) -> Option<String> {
        self.vim.as_ref().map(Vim::label)
    }
    /// A block cursor: vim mode in a non-insert mode.
    pub fn block_cursor(&self) -> bool {
        self.vim.as_ref().is_some_and(Vim::block_cursor)
    }
    /// Give `key` to vim mode; [`Outcome::Pass`] when vim is off.
    pub fn vim_key(&mut self, key: KeyEvent) -> Outcome {
        match &mut self.vim {
            Some(v) => v.key(&mut self.buf, key),
            None => Outcome::Pass,
        }
    }
    /// A paste is coming: leave vim visual mode.
    pub fn before_paste(&mut self) {
        if let Some(v) = &mut self.vim {
            v.before_paste();
        }
    }
    /// The vim visual selection.
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.vim.as_ref().and_then(|v| v.visual_range(&self.buf))
    }
}
