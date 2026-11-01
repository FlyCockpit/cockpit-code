//! Markdown → styled lines, adapted from FlyCockpit's Cockpit.
//! Removed: copy provenance, math typesetting. Added: fenced-code
//! highlighting through [`crate::highlight`]. Soft wrapping is the caller's
//! job (see [`crate::wrap`]); table layout uses `width` to fit columns.
use crate::highlight::Highlighter;
use crate::rows::{Btn, Meta, Row};
use crate::theme::theme;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const TAB_STOP: usize = 4;

fn expand_tabs(text: &str, start_col: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut col = start_col;
    for ch in text.chars() {
        if ch == '\t' {
            let spaces = TAB_STOP - col % TAB_STOP;
            out.push_str(&" ".repeat(spaces));
            col += spaces;
        } else {
            out.push(ch);
            col += ch.width().unwrap_or(0);
        }
    }
    out
}

fn options() -> Options {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts
}

/// A rendered message: rows with metadata, plus its code blocks and the
/// source ranges of its tables (for the copy buttons).
#[derive(Default)]
pub struct Md {
    pub rows: Vec<Row>,
    pub codes: Vec<String>,
    pub tables: Vec<std::ops::Range<usize>>,
}

/// Parse `src` and return its rows wrapped to `width` (list items and
/// quotes with their hanging indent).
pub fn render(src: &str, width: usize) -> Vec<Line<'static>> {
    crate::rows::wrap_rows(render_md(src, width, false).rows, width, &[], &[])
        .into_iter()
        .map(|(l, _)| l)
        .collect()
}

/// Parse `src` into rows with metadata; `buttons` adds copy labels to code
/// fences and tables.
pub fn render_md(src: &str, width: usize, buttons: bool) -> Md {
    if src.trim().is_empty() {
        return Md::default();
    }
    let mut emitter = Emitter {
        width,
        buttons,
        number_widths: number_widths(src),
        ..Emitter::default()
    };
    for (event, range) in Parser::new_ext(src, options()).into_offset_iter() {
        if let Event::Start(Tag::Table(_)) = &event {
            emitter.tables.push(range);
        }
        emitter.handle(event);
    }
    let tables = std::mem::take(&mut emitter.tables);
    let codes = std::mem::take(&mut emitter.codes);
    Md {
        rows: emitter.finish(),
        codes,
        tables,
    }
}

/// For each list in document order, the display width of its largest
/// item number (1 for bullet lists).
fn number_widths(src: &str) -> Vec<usize> {
    let mut out = vec![];
    let mut stack: Vec<(usize, Option<u64>, u64)> = vec![];
    for event in Parser::new_ext(src, options()) {
        match event {
            Event::Start(Tag::List(start)) => {
                stack.push((out.len(), start, 0));
                out.push(1);
            }
            Event::Start(Tag::Item) => {
                if let Some(top) = stack.last_mut() {
                    top.2 += 1;
                }
            }
            Event::End(TagEnd::List(_)) => {
                if let Some((i, Some(start), n)) = stack.pop() {
                    out[i] = (start + n.saturating_sub(1)).to_string().len();
                }
            }
            _ => {}
        }
    }
    out
}

/// Plain text with `code` spans styled as inline code (backticks dropped);
/// for short labels that are not rendered as full markdown.
pub fn inline_code(text: &str, base: Style) -> Vec<Span<'static>> {
    let t = theme();
    let code = Style::default().fg(t.code_fg).bg(t.code_bg);
    let parts: Vec<&str> = text.split('`').collect();
    // An unpaired backtick leaves the text as it is.
    if parts.len().is_multiple_of(2) {
        return vec![Span::styled(text.to_string(), base)];
    }
    parts
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.is_empty())
        .map(|(i, p)| Span::styled(p.to_string(), if i % 2 == 1 { code } else { base }))
        .collect()
}

/// Inline formatting for labels and notices, without block layout or copy buttons.
pub fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let t = theme();
    let mut styles = vec![base];
    let mut spans = vec![];
    for event in Parser::new_ext(text, options()) {
        let current = *styles.last().unwrap();
        match event {
            Event::Start(tag) => {
                let style = match tag {
                    Tag::Strong => current.add_modifier(Modifier::BOLD),
                    Tag::Emphasis => current.add_modifier(Modifier::ITALIC),
                    Tag::Strikethrough => current.add_modifier(Modifier::CROSSED_OUT),
                    Tag::Link { .. } => current.fg(t.link).add_modifier(Modifier::UNDERLINED),
                    _ => continue,
                };
                styles.push(style);
            }
            Event::End(
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link,
            ) => {
                styles.pop();
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                spans.push(Span::styled(text.into_string(), current));
            }
            Event::Code(text) => spans.push(Span::styled(
                text.into_string(),
                current.fg(t.code_fg).bg(t.code_bg),
            )),
            Event::SoftBreak | Event::HardBreak => spans.push(Span::styled(" ", current)),
            _ => {}
        }
    }
    spans
}

/// Parse complete inline syntax before clipping the rendered display columns.
pub fn inline_truncated(text: &str, base: Style, width: usize) -> Vec<Span<'static>> {
    crate::wrap::truncate_spans(inline(text, base), width)
}

/// HTML for `src` (copy rich / copy html).
pub fn html(src: &str) -> String {
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, Parser::new_ext(src, options()));
    out
}

/// Markdown rendered to plain text: no wraps, no borders or fences.
pub fn plain(src: &str) -> String {
    crate::rows::rows_text(&render_md(src, 100_000, false).rows)
}

#[derive(Default)]
struct Emitter {
    width: usize,
    buttons: bool,
    codes: Vec<String>,
    tables: Vec<std::ops::Range<usize>>,
    lines: Vec<Row>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    code: Option<Highlighter>,
    in_block_quote: bool,
    list_stack: Vec<Option<u64>>,
    /// Hanging indent (indent + marker width) of each open list item.
    item_hangs: Vec<usize>,
    /// The next flushed line starts with its item's marker.
    item_first: bool,
    /// Per list, in document order: the width of its largest number
    /// (ordered lists right-align "9." under "10.").
    number_widths: Vec<usize>,
    lists_seen: usize,
    /// The open lists' number widths.
    list_widths: Vec<usize>,
    table: Option<TableState>,
    /// The open link: its target and the text seen so far.
    link: Option<(String, String)>,
}

#[derive(Default)]
struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<TableRow>,
    current_row: Option<TableRow>,
    current_cell: Option<Vec<Span<'static>>>,
    in_head: bool,
}

struct TableRow {
    cells: Vec<Vec<Span<'static>>>,
    is_header: bool,
}

impl Emitter {
    fn handle(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(s) => {
                if let Some((_, text)) = &mut self.link {
                    text.push_str(&s);
                }
                self.text(s.into_string())
            }
            Event::Code(s) => {
                if let Some((_, text)) = &mut self.link {
                    text.push_str(&s);
                }
                self.inline_code(s.into_string())
            }
            Event::SoftBreak => self.text(" ".to_string()),
            Event::HardBreak => self.flush_line(),
            Event::Rule => self.horizontal_rule(),
            Event::Html(s) | Event::InlineHtml(s) => self.text(s.into_string()),
            Event::InlineMath(s) => self.text(format!("${s}$")),
            Event::DisplayMath(s) => self.text(format!("$${s}$$")),
            Event::TaskListMarker(done) => {
                let t = theme();
                let (mark, style) = if done {
                    ("☑ ", Style::default().fg(t.green))
                } else {
                    ("☐ ", Style::default().fg(t.fog))
                };
                // Replace the bullet just pushed by the list item.
                if let Some(last) = self.current.last_mut()
                    && last.content.ends_with("• ")
                {
                    let indent = last.content.trim_end_matches("• ").to_string();
                    *last = Span::raw(indent);
                }
                self.current.push(Span::styled(mark, style));
            }
            Event::FootnoteReference(_) => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        let t = theme();
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line();
                let style = Style::default().fg(t.heading).add_modifier(Modifier::BOLD);
                // The level marks stay (they say the depth) but recede, so
                // the heading text reads first.
                if heading_depth(level) > 1 {
                    self.current.push(Span::styled(
                        "#".repeat(heading_depth(level)) + " ",
                        Style::default().fg(t.dim),
                    ));
                }
                self.push_style(style);
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.in_block_quote = true;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let label = format!("╭ {}", if lang.is_empty() { "code" } else { &lang });
                let mut meta = Meta::skip();
                let mut spans = vec![Span::styled(label.clone(), Style::default().fg(t.dim))];
                if self.buttons {
                    let x = label.width() + 2;
                    let b = Btn::Code(self.codes.len());
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(b.label(), Style::default().fg(t.dim)));
                    meta.buttons
                        .push((x as u16, (x + b.label().width()) as u16, b));
                }
                self.lines.push((Line::from(spans), meta));
                self.codes.push(String::new());
                self.code = Some(Highlighter::new(&lang));
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(start);
                let w = self
                    .number_widths
                    .get(self.lists_seen)
                    .copied()
                    .unwrap_or(1);
                self.list_widths.push(w);
                self.lists_seen += 1;
            }
            Tag::Table(alignments) => {
                self.flush_line();
                self.table = Some(TableState {
                    alignments,
                    ..TableState::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = true;
                    table.current_row = Some(TableRow {
                        cells: Vec::new(),
                        is_header: true,
                    });
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table
                    && table.current_row.is_none()
                {
                    table.current_row = Some(TableRow {
                        cells: Vec::new(),
                        is_header: table.in_head,
                    });
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.current_cell = Some(Vec::new());
                }
            }
            Tag::Item => {
                self.flush_line();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let w = self.list_widths.last().copied().unwrap_or(1);
                        let m = format!("{n:>w$}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                let lead = format!("{indent}{marker}");
                self.item_hangs.push(lead.width());
                self.item_first = true;
                self.current
                    .push(Span::styled(lead, Style::default().fg(t.fog)));
            }
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT))
            }
            Tag::Link { dest_url, .. } => {
                self.link = Some((dest_url.into_string(), String::new()));
                self.push_style(
                    Style::default()
                        .fg(t.link)
                        .add_modifier(Modifier::UNDERLINED),
                )
            }
            Tag::Image { .. } => self.push_style(Style::default().fg(t.fog)),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_line_then_blank(),
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flush_line_then_blank();
            }
            TagEnd::BlockQuote(_) => {
                self.in_block_quote = false;
                self.flush_line_then_blank();
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.code = None;
                self.blank();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.list_widths.pop();
                if self.list_stack.is_empty() {
                    self.flush_line_then_blank();
                } else {
                    self.flush_line();
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                    self.blank();
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    if let Some(row) = table.current_row.take() {
                        table.rows.push(row);
                    }
                    table.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table
                    && let Some(row) = table.current_row.take()
                {
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table
                    && let Some(cell) = table.current_cell.take()
                    && let Some(row) = &mut table.current_row
                {
                    row.cells.push(cell);
                }
            }
            TagEnd::Item => {
                self.flush_line();
                self.item_hangs.pop();
                self.item_first = false;
            }
            TagEnd::Link => {
                self.pop_style();
                // The target, dim, when the text does not already say it
                // (it is what a click opens, and stays readable when copied).
                if let Some((url, text)) = self.link.take()
                    && !url.is_empty()
                    && !url.starts_with('#')
                    && url.trim_start_matches("mailto:") != text.trim()
                    && url != text.trim()
                {
                    let t = theme();
                    self.text_styled(format!(" ({url})"), Style::default().fg(t.dim));
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Image => {
                self.pop_style()
            }
            _ => {}
        }
    }

    fn text(&mut self, s: String) {
        if let Some(table) = &mut self.table {
            let style = self.style_stack.last().copied().unwrap_or_default();
            if let Some(cell) = &mut table.current_cell {
                let start: usize = cell.iter().map(|s| s.content.width()).sum();
                cell.push(Span::styled(
                    expand_tabs(&s.replace('\n', " "), start),
                    style,
                ));
            }
            return;
        }
        if let Some(hl) = &mut self.code {
            if let Some(code) = self.codes.last_mut() {
                code.push_str(&s);
            }
            let t = theme();
            let gutter = Span::styled("│ ", Style::default().fg(t.dim));
            for raw in s.split_inclusive('\n') {
                let line = raw.strip_suffix('\n').unwrap_or(raw);
                let expanded = expand_tabs(line, 0);
                let mut spans = vec![gutter.clone()];
                spans.extend(hl.line(&expanded, Style::default()));
                // A wrapped code line keeps the fence gutter.
                self.lines.push((
                    Line::from(spans),
                    Meta {
                        cont: vec![gutter.clone()],
                        ..Meta::gutter(2)
                    },
                ));
            }
            return;
        }
        // An item whose text starts with its own status mark ("☐ …") shows
        // the mark in place of the bullet, like a task list.
        if self.item_first
            && self.current.len() == 1
            && self.current[0].content.ends_with("• ")
            && s.starts_with(['☐', '☑', '☒', '✓', '✗', '◐', '⊘', '◆'])
        {
            let indent = self.current[0].content.trim_end_matches("• ").to_string();
            self.current[0] = Span::raw(indent);
        }
        let style = self.current_style();
        let mut first = true;
        for piece in s.split('\n') {
            if !first {
                self.flush_line();
            }
            if !piece.is_empty() {
                let start = self.current.iter().map(|span| span.content.width()).sum();
                self.current
                    .push(Span::styled(expand_tabs(piece, start), style));
            }
            first = false;
        }
    }

    fn inline_code(&mut self, s: String) {
        let t = theme();
        let style = Style::default().fg(t.code_fg).bg(t.code_bg);
        if let Some(table) = &mut self.table {
            if let Some(cell) = &mut table.current_cell {
                cell.push(Span::styled(s, style));
            }
            return;
        }
        self.current.push(Span::styled(s, style));
    }

    fn horizontal_rule(&mut self) {
        self.flush_line();
        let w = self.width.clamp(1, 40);
        self.lines.push((
            Line::from(Span::styled(
                "─".repeat(w),
                Style::default().fg(theme().rule),
            )),
            Meta::skip(),
        ));
        self.blank();
    }

    /// Text in a given style (not the current one).
    fn text_styled(&mut self, s: String, style: Style) {
        if let Some(table) = &mut self.table {
            if let Some(cell) = &mut table.current_cell {
                cell.push(Span::styled(s, style));
            }
            return;
        }
        self.style_stack.push(style);
        self.text(s);
        self.style_stack.pop();
    }

    fn push_style(&mut self, style: Style) {
        let merged = self.current_style().patch(style);
        self.style_stack.push(merged);
    }

    fn pop_style(&mut self) {
        self.style_stack.pop();
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn emit_table(&mut self, table: TableState) {
        let column_count = table
            .rows
            .iter()
            .map(|row| row.cells.len())
            .max()
            .unwrap_or(0)
            .max(table.alignments.len());
        if column_count == 0 {
            return;
        }
        // A table inside a block quote keeps the quote bar on every row.
        let bar = if self.in_block_quote { 2 } else { 0 };
        let widths = table_column_widths(&table, column_count, self.width.saturating_sub(bar));
        let first = self.lines.len();
        self.push_border("┌", "┬", "┐", &widths);
        for (idx, row) in table.rows.iter().enumerate() {
            self.push_table_row(row, &table.alignments, &widths);
            if row.is_header && table.rows.get(idx + 1).is_some() {
                self.push_border("├", "┼", "┤", &widths);
            }
        }
        self.push_border("└", "┴", "┘", &widths);
        if bar > 0 {
            let quote = Span::styled("▌ ", Style::default().fg(theme().fog));
            for (line, meta) in &mut self.lines[first..] {
                line.spans.insert(0, quote.clone());
                meta.gutter += 2;
            }
        }
        if self.buttons {
            let t = theme();
            let n = self.tables.len().saturating_sub(1);
            let row = crate::rows::button_row(
                " ",
                &[Btn::TableMd(n), Btn::TableHtml(n)],
                Style::default().fg(t.dim),
            );
            let used = crate::rows::width(&self.lines.last().unwrap().0.spans);
            let extra = crate::rows::width(&row.0.spans);
            if used + extra <= self.width {
                // On the bottom border when it fits.
                let (line, meta) = self.lines.last_mut().unwrap();
                line.spans.extend(row.0.spans);
                meta.buttons = row
                    .1
                    .buttons
                    .iter()
                    .map(|&(a, b, x)| (a + used as u16, b + used as u16, x))
                    .collect();
            } else {
                self.lines.push(row);
            }
        }
    }

    fn push_border(&mut self, left: &str, junction: &str, right: &str, widths: &[usize]) {
        let mut text = String::from(left);
        for (idx, width) in widths.iter().enumerate() {
            text.push_str(&"─".repeat(width + 2));
            text.push_str(if idx + 1 == widths.len() {
                right
            } else {
                junction
            });
        }
        self.lines.push((
            Line::from(Span::styled(text, Style::default().fg(theme().rule))),
            Meta::skip(),
        ));
    }

    fn push_table_row(&mut self, row: &TableRow, alignments: &[Alignment], widths: &[usize]) {
        let border = Style::default().fg(theme().rule);
        let header = if row.is_header {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let wrapped: Vec<Vec<Vec<Span<'static>>>> = widths
            .iter()
            .enumerate()
            .map(|(idx, w)| {
                let cell = row.cells.get(idx).cloned().unwrap_or_default();
                wrap_spans_to_width(&cell, *w)
            })
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for visual in 0..height {
            let mut spans = vec![Span::styled("│", border)];
            for (col, width) in widths.iter().enumerate() {
                let cell_line = wrapped[col].get(visual).cloned().unwrap_or_default();
                let cell_width = spans_width(&cell_line);
                let slack = width.saturating_sub(cell_width);
                let (lp, rp) = match alignments.get(col).copied().unwrap_or(Alignment::None) {
                    Alignment::Right => (slack, 0),
                    Alignment::Center => (slack / 2, slack - slack / 2),
                    Alignment::Left | Alignment::None => (0, slack),
                };
                spans.push(Span::raw(" ".repeat(lp + 1)));
                spans.extend(cell_line.into_iter().map(|s| {
                    let style = s.style.patch(header);
                    s.style(style)
                }));
                spans.push(Span::raw(" ".repeat(rp + 1)));
                spans.push(Span::styled("│", border));
            }
            self.lines.push((
                Line::from(spans),
                Meta {
                    table: true,
                    ..Meta::default()
                },
            ));
        }
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.current);
        let mut lead: Vec<Span<'static>> = vec![];
        let mut cont: Vec<Span<'static>> = vec![];
        // List text wraps under its marker; later lines of an item (a
        // second paragraph, a hard break) align with it too.
        if let Some(&hang) = self.item_hangs.last() {
            if !self.item_first {
                lead.push(Span::raw(" ".repeat(hang)));
            }
            cont.push(Span::raw(" ".repeat(hang)));
            self.item_first = false;
        }
        let line = if self.in_block_quote {
            let bar = Span::styled("▌ ", Style::default().fg(theme().fog));
            lead.push(bar.clone());
            cont.push(bar);
            let gutter = crate::rows::width(&lead);
            lead.extend(spans.into_iter().map(|s| {
                let style = Style::default()
                    .fg(theme().fog)
                    .add_modifier(Modifier::ITALIC)
                    .patch(s.style);
                s.style(style)
            }));
            (Line::from(lead), gutter)
        } else {
            let gutter = crate::rows::width(&lead);
            lead.extend(spans);
            (Line::from(lead), gutter)
        };
        self.lines.push((
            line.0,
            Meta {
                gutter: line.1 as u16,
                cont,
                ..Meta::default()
            },
        ));
    }

    fn blank(&mut self) {
        if !matches!(self.lines.last(), Some((l, _)) if l.spans.is_empty()) {
            self.lines.push((Line::default(), Meta::default()));
        }
    }

    fn flush_line_then_blank(&mut self) {
        self.flush_line();
        if self.list_stack.is_empty() {
            self.blank();
        }
    }

    fn finish(mut self) -> Vec<Row> {
        self.flush_line();
        while matches!(self.lines.last(), Some((l, _)) if l.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

fn table_column_widths(table: &TableState, column_count: usize, available: usize) -> Vec<usize> {
    let natural: Vec<usize> = (0..column_count)
        .map(|idx| {
            table
                .rows
                .iter()
                .filter_map(|row| row.cells.get(idx))
                .map(|cell| spans_width(cell))
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();
    let budget = available.saturating_sub(column_count * 3 + 1);
    if budget >= natural.iter().sum::<usize>() {
        return natural;
    }
    if budget < column_count {
        return vec![1; column_count];
    }
    let mut widths = vec![1; column_count];
    let mut remaining = budget - column_count;
    while remaining > 0 {
        let mut grown = false;
        for idx in 0..column_count {
            if remaining == 0 {
                break;
            }
            if widths[idx] < natural[idx] {
                widths[idx] += 1;
                remaining -= 1;
                grown = true;
            }
        }
        if !grown {
            break;
        }
    }
    widths
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

fn wrap_spans_to_width(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let line = Line::from(spans.to_vec());
    crate::wrap::wrap_lines(vec![line], width.max(1), &[], &[])
        .into_iter()
        .map(|l| l.spans)
        .collect()
}

fn heading_depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
