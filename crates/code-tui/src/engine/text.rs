//! Engine text without what is meant for the model: context tags,
//! guidance notices and injection wrappers.

/// Tool output without appended guidance notices (a
/// `<system-message>` wrapping `<guidance-selection>`): the transcript shows
/// them as their own row.
pub fn strip_guidance(s: String) -> String {
    const OPEN: &str = "<system-message id=\"";
    if !s.contains("<guidance-selection id=") {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(at) = rest.find(OPEN) {
        let after = &rest[at + OPEN.len()..];
        let Some(q) = after.find("\">") else { break };
        let body = after[q + 2..].trim_start();
        let close = format!("</system-message id=\"{}\">", &after[..q]);
        match after.find(&close) {
            Some(end) if body.starts_with("<guidance-selection id=") => {
                out.push_str(rest[..at].trim_end_matches('\n'));
                rest = &after[end + close.len()..];
            }
            _ => {
                out.push_str(&rest[..at + OPEN.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Text without `<ctxPercentage id="R">N</ctxPercentage id="R">` tags:
/// they are for the model; the status line shows context use.
pub(super) fn strip_ctx(s: &str) -> String {
    const OPEN: &str = "<ctxPercentage id=\"";
    if !s.contains(OPEN) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(OPEN) {
        let after = &rest[at + OPEN.len()..];
        let Some(q) = after.find('"') else { break };
        let close = format!("</ctxPercentage id=\"{}\">", &after[..q]);
        let Some(end) = after.find(&close) else { break };
        out.push_str(rest[..at].trim_end_matches([' ', '\n']));
        rest = &after[end + close.len()..];
    }
    out.push_str(rest);
    out
}

/// A compaction summary without its injection wrapper: the old context's
/// copy and the new context's head carry different random ids, so
/// the bare text identifies the summary and is what is shown.
pub(super) fn summary_text(s: &str) -> String {
    let t = s.trim();
    if let Some(after) = t.strip_prefix("<system-message id=\"")
        && let Some(q) = after.find("\">")
    {
        let close = format!("</system-message id=\"{}\">", &after[..q]);
        if let Some(end) = after.find(&close) {
            return after[q + 2..end].trim().to_string();
        }
    }
    t.to_string()
}
