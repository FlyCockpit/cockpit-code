//! Eight-stage edit matching cascade adapted from FlyCockpit's Cockpit.
use anyhow::{Result, bail};
use std::ops::Range;
fn invalid_input(message: &str) -> anyhow::Error {
    anyhow::anyhow!("{message}")
}
pub struct Match {
    pub canonical: String,
    pub spans: Vec<Range<usize>>,
    pub stage: &'static str,
}
pub fn find_match(file: &str, target: &str, replace_all: bool) -> Result<Option<Match>> {
    if target.is_empty() {
        return Err(invalid_input("`old_string` must not be empty"));
    }

    // Stage 1 — exact.
    if file.contains(target) {
        let spans: Vec<Range<usize>> = file
            .match_indices(target)
            .map(|(start, m)| start..start + m.len())
            .collect();
        if !replace_all && spans.len() > 1 {
            return Err(invalid_input(
                "Found multiple matches for `old_string`; pass more surrounding context or set replace_all: true",
            ));
        }
        return Ok(Some(Match {
            canonical: target.to_string(),
            spans: if replace_all {
                spans
            } else {
                spans.into_iter().take(1).collect()
            },
            stage: "exact",
        }));
    }

    // Stage 2 — line-trim.
    if let Some(spans) = match_via_normalizer(file, target, replace_all, line_trim_normalize)? {
        return Ok(Some(Match {
            canonical: file[spans[0].clone()].to_string(),
            spans,
            stage: "line_trim",
        }));
    }

    // Stage 3 — block-anchor (anchored region with ≥90% interior overlap).
    if let Some(spans) = anchor_match(file, target, 90, replace_all)? {
        return Ok(Some(Match {
            canonical: file[spans[0].clone()].to_string(),
            spans,
            stage: "block_anchor",
        }));
    }

    // Stage 4 — whitespace-normalized (collapse runs).
    if let Some(spans) = match_via_normalizer(file, target, replace_all, whitespace_collapse)? {
        return Ok(Some(Match {
            canonical: file[spans[0].clone()].to_string(),
            spans,
            stage: "whitespace_normalized",
        }));
    }

    // Stage 5 — indent-flexible (strip common leading indentation from both).
    if let Some(spans) = match_via_normalizer(file, target, replace_all, indent_flexible_normalize)?
    {
        return Ok(Some(Match {
            canonical: file[spans[0].clone()].to_string(),
            spans,
            stage: "indent_flexible",
        }));
    }

    // Stage 6 — escape-normalized.
    if let Some(spans) = match_via_normalizer(file, target, replace_all, escape_normalize)? {
        return Ok(Some(Match {
            canonical: file[spans[0].clone()].to_string(),
            spans,
            stage: "escape_normalized",
        }));
    }

    // Stage 7 — trimmed-boundary (trim outer whitespace of the whole block).
    if let Some(spans) = match_via_normalizer(file, target, replace_all, trim_boundary_normalize)? {
        return Ok(Some(Match {
            canonical: file[spans[0].clone()].to_string(),
            spans,
            stage: "trimmed_boundary",
        }));
    }

    // Stage 8 — context-aware (anchored region with ≥50% interior overlap;
    // the looser cousin of stage 3).
    if let Some(spans) = anchor_match(file, target, 50, replace_all)? {
        return Ok(Some(Match {
            canonical: file[spans[0].clone()].to_string(),
            spans,
            stage: "context_aware",
        }));
    }

    Ok(None)
}

/// Generic "normalize both sides and find" stage. The normalizer maps
/// chunks of bytes onto a canonical form; we slide a window of the
/// same shape over the file and compare normalized forms. On a match
/// we return the *original file bytes* that produced the equivalent
/// normalized form.
fn match_via_normalizer(
    file: &str,
    target: &str,
    replace_all: bool,
    normalize: fn(&str) -> String,
) -> Result<Option<Vec<Range<usize>>>> {
    let norm_target = normalize(target);
    if norm_target.trim().is_empty() {
        return Ok(None);
    }

    // We brute-force: for each newline-delimited substring of the file
    // that's the same line count as `target`, compare its normalized
    // form against `norm_target`.
    let target_lines = target.matches('\n').count() + 1;
    let mut file_lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for line in file.split_inclusive('\n') {
        file_lines.push((offset, line));
        offset += line.len();
    }
    if file_lines.len() < target_lines {
        return Ok(None);
    }

    let mut hits: Vec<Range<usize>> = Vec::new();
    let mut last_end = 0usize;
    for start in 0..=file_lines.len() - target_lines {
        let span_start = file_lines[start].0;
        let candidate: String = file_lines[start..start + target_lines]
            .iter()
            .map(|(_, line)| *line)
            .collect();
        // Strip the trailing newline that split_inclusive kept iff target
        // didn't have one — match equivalence has to compare like with like.
        let cand_for_compare = if target.ends_with('\n') {
            candidate.clone()
        } else {
            candidate
                .strip_suffix('\n')
                .map(str::to_string)
                .unwrap_or_else(|| candidate.clone())
        };
        let norm = normalize(&cand_for_compare);
        if norm == norm_target {
            let span = span_start..span_start + cand_for_compare.len();
            if !replace_all && !hits.is_empty() {
                return Err(invalid_input(
                    "Found multiple matches for `old_string` at normalized stage; pass more surrounding context or set replace_all: true",
                ));
            }
            if replace_all && span.start < last_end {
                continue;
            }
            last_end = span.end;
            hits.push(span);
        }
    }
    if replace_all {
        Ok((!hits.is_empty()).then_some(hits))
    } else {
        Ok(hits.into_iter().next().map(|span| vec![span]))
    }
}

pub fn replace_spans(file: &str, spans: &[Range<usize>], replacement: &str) -> Result<String> {
    if spans.is_empty() {
        bail!("internal error: matched stage produced no spans");
    }
    let mut out = String::with_capacity(file.len());
    let mut cursor = 0usize;
    for span in spans {
        if span.start < cursor
            || span.end < span.start
            || span.end > file.len()
            || !file.is_char_boundary(span.start)
            || !file.is_char_boundary(span.end)
        {
            bail!("internal error: matched stage produced invalid replacement span");
        }
        out.push_str(&file[cursor..span.start]);
        out.push_str(replacement);
        cursor = span.end;
    }
    out.push_str(&file[cursor..]);
    Ok(out)
}

fn line_trim_normalize(s: &str) -> String {
    s.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
}

fn whitespace_collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim().to_string()
}

fn indent_flexible_normalize(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.bytes().take_while(|b| *b == b' ' || *b == b'\t').count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.len() >= min_indent && l.is_char_boundary(min_indent) {
                &l[min_indent..]
            } else {
                *l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_normalize(s: &str) -> String {
    s.replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
}

fn trim_boundary_normalize(s: &str) -> String {
    s.trim().to_string()
}

/// Anchor-based match shared by stages 3 and 8. Pin candidate regions
/// by exact first + last lines, then accept only candidates whose
/// interior char overlap with `target` meets `min_ratio` percent. The
/// caller picks the threshold: 90 for block-anchor (stage 3), 50 for
/// context-aware (stage 8). Among acceptable candidates, the one with
/// the highest overlap wins.
///
/// Char overlap is a cheap proxy for Levenshtein — sufficient for "is
/// this region similar?" without pulling in an extra crate.
fn anchor_match(
    file: &str,
    target: &str,
    min_ratio: usize,
    replace_all: bool,
) -> Result<Option<Vec<Range<usize>>>> {
    if file.is_empty() {
        return Ok(None);
    }
    let target_lines: Vec<&str> = target.lines().collect();
    if target_lines.len() < 2 {
        return Ok(None);
    }
    let first = target_lines.first().unwrap().trim();
    let last = target_lines.last().unwrap().trim();
    if first.is_empty() || last.is_empty() {
        return Ok(None);
    }

    let file_lines: Vec<&str> = file.split_inclusive('\n').collect();
    let n = target_lines.len();
    let mut hits: Vec<Range<usize>> = Vec::new();
    let mut offset = 0;

    for start in 0..=file_lines.len().saturating_sub(n) {
        if start > 0 {
            offset += file_lines[start - 1].len();
        }
        let cand_first = file_lines[start].trim_end_matches('\n').trim();
        if cand_first != first {
            continue;
        }
        let cand_last_idx = start + n - 1;
        if cand_last_idx >= file_lines.len() {
            continue;
        }
        let cand_last = file_lines[cand_last_idx].trim_end_matches('\n').trim();
        if cand_last != last {
            continue;
        }

        let candidate: String = file_lines[start..start + n].concat();
        let cand_for_compare = if target.ends_with('\n') {
            candidate.clone()
        } else {
            candidate
                .strip_suffix('\n')
                .map(str::to_string)
                .unwrap_or_else(|| candidate.clone())
        };

        let target_chars: std::collections::HashMap<char, usize> = char_counts(target);
        let cand_chars: std::collections::HashMap<char, usize> = char_counts(&cand_for_compare);
        let common: usize = target_chars
            .iter()
            .map(|(c, n)| n.min(cand_chars.get(c).unwrap_or(&0)))
            .copied()
            .sum();
        let denom = target.chars().count().max(1);
        let ratio = common * 100 / denom;

        if ratio < min_ratio {
            continue;
        }
        if !replace_all && !hits.is_empty() {
            bail!(
                "Found multiple acceptable anchor matches; supply unique context or replace_all: true"
            );
        }
        let span = offset..offset + cand_for_compare.len();
        if hits.last().is_none_or(|last| span.start >= last.end) {
            hits.push(span);
        }
    }

    Ok((!hits.is_empty()).then_some(hits))
}

fn char_counts(s: &str) -> std::collections::HashMap<char, usize> {
    let mut m = std::collections::HashMap::new();
    for c in s.chars() {
        *m.entry(c).or_insert(0) += 1;
    }
    m
}

/// Return the file region nearest to `target` (by char overlap), at
/// most ~10 lines, for the "no match" error message.
pub fn nearest_miss(file: &str, target: &str) -> String {
    let target_lines = target.lines().count().max(1);
    let file_lines: Vec<&str> = file.split_inclusive('\n').collect();
    if file_lines.len() < target_lines {
        return file.to_string();
    }
    let target_counts = char_counts(target);
    let mut best: Option<(usize, usize)> = None;
    for start in 0..=file_lines.len() - target_lines {
        let cand: String = file_lines[start..start + target_lines].concat();
        let cand_counts = char_counts(&cand);
        let common: usize = target_counts
            .iter()
            .map(|(c, n)| n.min(cand_counts.get(c).unwrap_or(&0)))
            .copied()
            .sum();
        if best.as_ref().map(|(_, s)| *s < common).unwrap_or(true) {
            best = Some((start, common));
        }
    }
    let Some((start, _)) = best else {
        return String::new();
    };
    let end = (start + target_lines).min(file_lines.len());
    file_lines[start..end].concat()
}
