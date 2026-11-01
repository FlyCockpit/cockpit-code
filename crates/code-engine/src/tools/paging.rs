//! Lossless UTF-8 pagination. Leave space for cursors and caller diagnostics below cap().
use anyhow::{Result, bail};
use serde_json::Value;

const BUDGET: usize = 7168;
pub(super) struct Page {
    output: String,
    offset: usize,
    start_byte: usize,
    limit: usize,
    seen: usize,
    emitted: usize,
    next: Option<(usize, usize)>,
}
impl Page {
    pub fn new(args: &Value, default_limit: usize) -> Self {
        Self {
            output: String::new(),
            offset: args["offset"].as_u64().unwrap_or(1).max(1) as usize,
            start_byte: args["start_byte"].as_u64().unwrap_or(0) as usize,
            limit: args["limit"]
                .as_u64()
                .unwrap_or(default_limit as u64)
                .clamp(1, 10000) as usize,
            seen: 0,
            emitted: 0,
            next: None,
        }
    }
    /// Prefixes are repeated for a split record; start_byte addresses only its text.
    pub fn push(&mut self, prefix: &str, text: &str) -> Result<bool> {
        self.seen += 1;
        if self.seen < self.offset {
            return Ok(true);
        }
        let start = if self.seen == self.offset {
            self.start_byte
        } else {
            0
        };
        if start > text.len() || !text.is_char_boundary(start) {
            bail!(
                "start_byte must be a UTF-8 boundary within the selected line ({} bytes)",
                text.len()
            );
        }
        let text_left = &text[start..];
        let available = BUDGET.saturating_sub(self.output.len() + prefix.len() + 1);
        if self.emitted >= self.limit || (text_left.len() > available && !self.output.is_empty()) {
            self.next = Some((self.seen, start));
            return Ok(false);
        }
        if prefix.len() + 1 >= BUDGET {
            bail!("entry path is too long to display; narrow the search path");
        }
        let mut end = text_left.len().min(available);
        while !text_left.is_char_boundary(end) {
            end -= 1;
        }
        self.output.push_str(prefix);
        self.output.push_str(&text_left[..end]);
        self.output.push('\n');
        self.emitted += 1;
        if end < text_left.len() {
            self.next = Some((self.seen, start + end));
            return Ok(false);
        }
        Ok(true)
    }
    pub fn finish(mut self, empty: &str) -> String {
        if self.output.is_empty() {
            self.output.push_str(empty);
        }
        if let Some((offset, byte)) = self.next {
            if byte == 0 {
                self.output
                    .push_str(&format!("\n[next offset: {offset}; start_byte: 0]"));
            } else {
                self.output.push_str(&format!("\n[next offset: {offset}; start_byte: {byte}; oversized line continues at this UTF-8 byte offset]"));
            }
        }
        self.output
    }
}
