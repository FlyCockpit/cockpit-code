//! Word-aware styled wrapping, adapted from FlyCockpit's Cockpit
//! (wrap separators retained for copying; grapheme clustering simplified).
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Cluster zero-width marks, variation selectors and ZWJ sequences onto the
/// preceding scalar so wrapping never splits a visible glyph.
fn graphemes(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut join_next = false;
    for ch in text.chars() {
        let cp = ch as u32;
        let combining = (!ch.is_control() && ch.width().unwrap_or(0) == 0)
            || ch == '\u{200d}'
            || (0xFE00..=0xFE0F).contains(&cp)
            || (0x1F3FB..=0x1F3FF).contains(&cp);
        match out.last_mut() {
            Some(last) if combining || join_next => last.push(ch),
            _ => out.push(ch.to_string()),
        }
        join_next = ch == '\u{200d}';
    }
    out
}

/// Re-wrap styled lines at whitespace, hard-cutting only tokens wider than
/// the available width. `prefix` is prepended to every output row and
/// `cont_prefix` to continuation rows (instead of `prefix`).
pub fn wrap_lines(
    lines: Vec<Line<'static>>,
    width: usize,
    prefix: &[Span<'static>],
    cont_prefix: &[Span<'static>],
) -> Vec<Line<'static>> {
    wrap_lines_with_joins(lines, width, prefix, cont_prefix)
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

/// Alongside each row, retain whether whitespace separated it from the
/// preceding piece. A hard cut inside a long word needs no copy separator.
pub fn wrap_lines_with_joins(
    lines: Vec<Line<'static>>,
    width: usize,
    prefix: &[Span<'static>],
    cont_prefix: &[Span<'static>],
) -> Vec<(Line<'static>, bool)> {
    let pw = prefix.iter().map(|s| s.content.width()).sum::<usize>();
    let cw = cont_prefix.iter().map(|s| s.content.width()).sum::<usize>();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let style = line.style;
        let mut remaining = line.spans;
        let mut first = true;
        let mut space_before = false;
        loop {
            let (pre, w) = if first {
                (prefix, pw)
            } else {
                (cont_prefix, cw)
            };
            let avail = width.saturating_sub(w).max(1);
            let (head, tail) = slice_spans_at_width(remaining, avail);
            let next_space = head
                .iter()
                .rev()
                .flat_map(|s| s.content.chars().rev())
                .next()
                .is_some_and(char::is_whitespace)
                || tail
                    .as_ref()
                    .and_then(|t| t.iter().flat_map(|s| s.content.chars()).next())
                    .is_some_and(char::is_whitespace);
            let mut spans = pre.to_vec();
            spans.extend(head);
            out.push((Line::from(spans).style(style), space_before));
            first = false;
            space_before = next_space;
            match tail {
                Some(t) => remaining = trim_leading_space(t),
                None => break,
            }
        }
    }
    out
}

fn trim_leading_space(mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    if let Some(first) = spans.first_mut() {
        let trimmed = first.content.trim_start_matches(' ');
        if trimmed.len() != first.content.len() && !trimmed.is_empty() {
            first.content = trimmed.to_string().into();
        }
    }
    spans
}

pub fn slice_spans_at_width(
    spans: Vec<Span<'static>>,
    max_width: usize,
) -> (Vec<Span<'static>>, Option<Vec<Span<'static>>>) {
    let total: usize = spans.iter().map(|span| span.content.width()).sum();
    if total <= max_width || max_width == 0 {
        return (spans, None);
    }
    let text = spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let scalars = spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect::<Vec<_>>();
    let mut scalar_offset = 0usize;
    let flat = graphemes(&text)
        .into_iter()
        .map(|grapheme| {
            let end = scalar_offset + grapheme.chars().count();
            let styled = scalars.get(scalar_offset..end).unwrap_or_default().to_vec();
            scalar_offset = end;
            (grapheme, styled)
        })
        .collect::<Vec<_>>();
    let mut used = 0usize;
    let mut hard_split = flat.len();
    let mut whitespace_split = None;
    for (index, (grapheme, _)) in flat.iter().enumerate() {
        let width = grapheme.width();
        if index > 0 && used + width > max_width {
            hard_split = index;
            break;
        }
        used += width;
        if used > max_width {
            hard_split = index + 1;
            break;
        }
        if grapheme.chars().all(char::is_whitespace) {
            whitespace_split = Some(index + 1);
        }
    }
    let split_at = whitespace_split.unwrap_or(hard_split);
    let head = group_into_spans(&flat[..split_at]);
    let tail = group_into_spans(&flat[split_at..]);
    (head, (!tail.is_empty()).then_some(tail))
}

fn group_into_spans(graphemes: &[(String, Vec<(char, Style)>)]) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut current_style = None;
    let mut current_text = String::new();
    for (_, styled) in graphemes {
        for &(ch, style) in styled {
            match current_style {
                Some(current) if current == style => current_text.push(ch),
                _ => {
                    if let Some(current) = current_style.take() {
                        out.push(Span::styled(std::mem::take(&mut current_text), current));
                    }
                    current_style = Some(style);
                    current_text.push(ch);
                }
            }
        }
    }
    if let Some(style) = current_style
        && !current_text.is_empty()
    {
        out.push(Span::styled(current_text, style));
    }
    out
}

/// Clip already-rendered spans, preserving styles and complete visible glyphs.
pub fn truncate_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    if max == 0 {
        return vec![];
    }
    if spans.iter().map(|s| s.content.width()).sum::<usize>() <= max {
        return spans;
    }
    let text = spans.iter().map(|s| s.content.as_ref()).collect::<String>();
    let scalars = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |ch| (ch, s.style)))
        .collect::<Vec<_>>();
    let mut used = 0;
    let mut offset = 0;
    let mut head = vec![];
    for glyph in graphemes(&text) {
        if used + glyph.width() > max - 1 {
            break;
        }
        used += glyph.width();
        let end = offset + glyph.chars().count();
        head.push((glyph, scalars[offset..end].to_vec()));
        offset = end;
    }
    let mut out = group_into_spans(&head);
    let style = out
        .last()
        .or_else(|| spans.first())
        .map_or(Style::default(), |s| s.style);
    out.push(Span::styled("…", style));
    out
}

/// Truncate a string to `max` display columns, appending `…` when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}
