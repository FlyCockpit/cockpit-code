//! Per-row metadata for rendered transcript lines: what is decoration, where
//! wraps were inserted, which spans are clickable buttons. Selection copy,
//! copy buttons, header toggles and inner scroll boxes all read it, so
//! hit-testing never re-measures rendered text.
use crate::wrap::wrap_lines_with_joins;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A clickable label's action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Btn {
    /// Assistant message: markdown rendered to plain text.
    CopyText,
    /// Assistant message: HTML with a plain-text alternative.
    CopyRich,
    /// Assistant message: the raw markdown source.
    CopyMarkdown,
    /// The n-th table of a message as a GitHub markdown table.
    TableMd(usize),
    /// The n-th table of a message as HTML.
    TableHtml(usize),
    /// The n-th fenced code block, code only.
    Code(usize),
    /// A write/edit call as a unified diff.
    Diff,
    /// A past user message: fork before it.
    Fork,
    /// A past user message: copy its text.
    CopyUser,
    /// The n-th paste chip of a user message: view it.
    Paste(usize),
    /// The n-th attachment chip of a user message: open it (D79).
    Attachment(usize),
    /// Composer paste chip `.0`: view it read-only.
    ChipView(usize),
    /// Composer paste chip `.0`: edit just that paste in `$EDITOR`.
    ChipEdit(usize),
    /// Composer paste chip `.0`: replace it with its text.
    ChipExpand(usize),
    /// Composer paste chip `.0`: remove it.
    ChipRemove(usize),
    /// Composer attachment chip `.0`: open the file in the system viewer.
    ChipOpen(usize),
    /// An "answered via fork" row: reopen the fork's saved chat (D64).
    OpenFork,
    /// A tool's saved full output (D129): open it in the pager.
    Output,
}

impl Btn {
    pub fn label(self) -> &'static str {
        match self {
            Btn::CopyText => "copy text",
            Btn::CopyRich => "copy rich",
            Btn::CopyMarkdown => "copy markdown",
            Btn::TableMd(_) => "copy md",
            Btn::TableHtml(_) => "copy html",
            Btn::Code(_) => "copy code",
            Btn::Diff => "copy diff",
            Btn::Fork => "fork here",
            Btn::CopyUser => "copy",
            Btn::Paste(_) => "view paste",
            Btn::Attachment(_) => "open attachment",
            Btn::ChipView(_) => "view",
            Btn::ChipEdit(_) => "open in editor",
            Btn::ChipExpand(_) => "expand inline",
            Btn::ChipRemove(_) => "remove",
            Btn::ChipOpen(_) => "open",
            Btn::OpenFork => "open fork chat",
            Btn::Output => "open",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Meta {
    /// Leading display columns that are decoration (gutters, bars, padding).
    pub gutter: u16,
    /// Exclusive end of copyable text, before trailing decorations such as time.
    pub text_end: Option<u16>,
    /// A wrap continuation of the previous row (no line break when copied).
    pub soft: bool,
    /// The soft wrap consumed whitespace, rather than cutting inside a word.
    pub space_before: bool,
    /// Decoration only: borders, fence headers, button rows, indicators.
    pub skip: bool,
    /// A table row: `│` separators become tabs when copied.
    pub table: bool,
    /// Clicking this row toggles the entry (expand/collapse).
    pub header: bool,
    /// The row belongs to the entry's inner scroll box.
    pub boxed: bool,
    /// Clickable labels: (start column, end column, action).
    pub buttons: Vec<(u16, u16, Btn)>,
    /// Hanging indent: decoration repeated on this row's wrap
    /// continuations (list text aligns under its marker, quotes keep
    /// their bar).
    pub cont: Vec<Span<'static>>,
}

impl Meta {
    pub fn gutter(n: usize) -> Self {
        Self {
            gutter: n as u16,
            ..Self::default()
        }
    }
    pub fn skip() -> Self {
        Self {
            skip: true,
            ..Self::default()
        }
    }
    pub fn header(gutter: usize) -> Self {
        Self {
            gutter: gutter as u16,
            header: true,
            ..Self::default()
        }
    }
}

pub type Row = (Line<'static>, Meta);

pub fn width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Rows with plain metadata (`gutter` decoration columns).
pub fn plain(lines: Vec<Line<'static>>, gutter: usize) -> Vec<Row> {
    lines
        .into_iter()
        .map(|l| (l, Meta::gutter(gutter)))
        .collect()
}

/// Wrap rows like [`wrap_lines`], keeping metadata: the first piece keeps
/// its buttons and gains the prefix width; continuations are `soft`.
pub fn wrap_rows(
    rows: Vec<Row>,
    width_: usize,
    prefix: &[Span<'static>],
    cont: &[Span<'static>],
) -> Vec<Row> {
    let pw = width(prefix);
    let mut out = Vec::with_capacity(rows.len());
    for (line, meta) in rows {
        let hang: Vec<Span<'static>> = cont.iter().chain(&meta.cont).cloned().collect();
        let hw = width(&hang);
        let pieces = wrap_lines_with_joins(vec![line], width_, prefix, &hang);
        for (i, (piece, space_before)) in pieces.into_iter().enumerate() {
            let m = if i == 0 {
                Meta {
                    gutter: meta.gutter + pw as u16,
                    buttons: meta
                        .buttons
                        .iter()
                        .map(|&(a, b, x)| (a + pw as u16, b + pw as u16, x))
                        .collect(),
                    ..meta.clone()
                }
            } else {
                Meta {
                    gutter: hw as u16,
                    soft: true,
                    space_before,
                    buttons: vec![],
                    header: false,
                    ..meta.clone()
                }
            };
            out.push((piece, m));
        }
    }
    out
}

/// A dim row of buttons: `prefix` then `a · b · c`.
pub fn button_row(prefix: &str, buttons: &[Btn], style: ratatui::style::Style) -> Row {
    let mut spans = vec![Span::raw(prefix.to_string())];
    let mut x = prefix.width();
    let mut hits = vec![];
    for (i, b) in buttons.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", style));
            x += 3;
        }
        let label = b.label();
        hits.push((x as u16, (x + label.width()) as u16, *b));
        spans.push(Span::styled(label, style));
        x += label.width();
    }
    (
        Line::from(spans),
        Meta {
            skip: true,
            buttons: hits,
            ..Meta::default()
        },
    )
}

/// The text of one row between display columns `c0..c1`, without its gutter.
pub fn row_text(line: &Line, meta: &Meta, c0: usize, c1: usize) -> String {
    let c0 = c0.max(meta.gutter as usize);
    let c1 = c1.min(meta.text_end.map_or(usize::MAX, usize::from));
    let mut out = String::new();
    let mut col = 0;
    for span in &line.spans {
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if col >= c0 && col < c1 {
                out.push(ch);
            }
            col += w;
        }
    }
    if meta.table {
        let cells: Vec<&str> = out
            .split('│')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        return cells.join("\t");
    }
    out
}

/// Join rows into text: soft rows continue the previous line; decoration
/// rows are dropped; trailing spaces are trimmed per line.
pub fn join<'a>(
    rows: impl IntoIterator<Item = (&'a Line<'static>, &'a Meta, usize, usize)>,
) -> String {
    let mut lines: Vec<String> = vec![];
    for (line, meta, c0, c1) in rows {
        if meta.skip {
            continue;
        }
        let text = row_text(line, meta, c0, c1);
        let text = text.trim_end();
        match lines.last_mut() {
            Some(last) if meta.soft => {
                if meta.space_before && !last.is_empty() && !text.is_empty() {
                    last.push(' ');
                }
                last.push_str(if meta.space_before {
                    text.trim_start()
                } else {
                    text
                });
            }
            _ => lines.push(text.to_string()),
        }
    }
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    lines.join("\n").trim_matches('\n').to_string()
}

/// Plain text of whole rows.
pub fn rows_text(rows: &[Row]) -> String {
    join(rows.iter().map(|(l, m)| (l, m, 0, usize::MAX)))
}
