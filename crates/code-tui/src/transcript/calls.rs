//! Tool-call rows: the one-line head of a call (its key argument, run
//! time, status), its expanded body (diffs, output, script host calls,
//! reviews) and the live preview of a call still being written.
use super::{BoxInfo, Pending, SPINNER, Tool, boxed};
use crate::diff;
use crate::rows::{Btn, Meta, Row, plain, wrap_rows};
use crate::theme::theme;
use crate::wrap::{fmt_bytes, rel, truncate};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

const COLLAPSED_DIFF_ROWS: usize = 12;
/// Rows kept from a tool result before the box (the box scrolls within them).
const MAX_OUTPUT_LINES: usize = 5000;
/// Visible rows of an expanded tool result (Cockpit's `TOOLCALL_RESULT_VISIBLE`).
const RESULT_VISIBLE: usize = 20;
/// Visible rows of the live preview of a write/edit being written.
const PREVIEW_VISIBLE: usize = 8;

/// A write/edit result that is a scope request: (status, owner). The
/// no-wait form is `{requestId, owner, status, applied:false}`; a waited
/// one is the whole request (`requester`, `owner`, `status`).
fn scope_result(output: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(output.lines().next()?.trim()).ok()?;
    let is_request =
        v.get("requestId").is_some() || (v.get("requester").is_some() && v.get("call").is_some());
    if !is_request {
        return None;
    }
    let status = v["status"].as_str()?.to_string();
    let owner = v["owner"].as_str().unwrap_or("its owner").to_string();
    Some((status, owner))
}

/// The argument that identifies a call at a glance.
fn key_arg(tool: &Tool) -> String {
    call_arg(&tool.name, &tool.args)
}

/// [`key_arg`] for any call by name and arguments (script inner calls too).
fn call_arg(name: &str, a: &Value) -> String {
    if matches!(name, "mcp.search" | "mcp.describe") {
        return generic_arg(a);
    }
    // Script host functions such as `criteria.propose`: the tool's own
    // summary, without repeating the action the name already shows.
    if let Some((base, action)) = name.split_once('.')
        && a.get("action").is_none()
        && let Some(obj) = a.as_object()
    {
        let mut with = obj.clone();
        with.insert("action".into(), Value::String(action.into()));
        let full = call_arg(base, &Value::Object(with));
        return full
            .strip_prefix(action)
            .map(|r| r.trim_start().to_string())
            .unwrap_or(full);
    }
    let s = |k: &str| rel(a[k].as_str().unwrap_or_default());
    match name {
        "read" => {
            let mut p = s("path");
            if let Some(o) = a["offset"].as_u64().filter(|&o| o > 1) {
                p.push_str(&format!(":{o}"));
            }
            p
        }
        "write" | "edit" => s("path"),
        "shell" => s("command").lines().next().unwrap_or_default().to_string(),
        "bash" => {
            let cmd = s("command").lines().next().unwrap_or_default().to_string();
            if a["background"].as_bool() == Some(true) {
                format!("{cmd}  (background)")
            } else {
                cmd
            }
        }
        "grep" => {
            let path = s("path");
            if path.is_empty() {
                s("pattern")
            } else {
                format!("{}  in {path}", s("pattern"))
            }
        }
        "glob" => s("pattern"),
        "script" => {
            let code = a["code"].as_str().unwrap_or_default();
            let first = code
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or_default();
            if a["background"].as_bool() == Some(true) {
                format!("{}  (background)", first.trim())
            } else {
                first.trim().to_string()
            }
        }
        // The child's durable title, else the brief's first line.
        "task" => {
            let title = s("title");
            if title.trim().is_empty() {
                s("prompt").lines().next().unwrap_or_default().to_string()
            } else {
                title
            }
        }
        // `criteria {action, id?, text?, evidence?, kind?}`.
        "criteria" => {
            let action = a["action"].as_str().unwrap_or("show");
            let id = a["id"]
                .as_str()
                .map(|i| format!("{} ", i.chars().take(8).collect::<String>()))
                .unwrap_or_default();
            let kind = a["kind"]
                .as_str()
                .map(|k| format!("{k} "))
                .unwrap_or_default();
            let detail = [
                "text",
                "evidence",
                "change",
                "reason",
                "objective",
                "status",
            ]
            .iter()
            .find_map(|k| a[*k].as_str())
            .unwrap_or_default();
            format!(
                "{action} {kind}{id}{}",
                detail.lines().next().unwrap_or_default()
            )
            .trim_end()
            .to_string()
        }
        // `question {questions: [{text, ...}]}`: the first question.
        "question" => {
            let qs = a["questions"].as_array().cloned().unwrap_or_default();
            let first = qs
                .first()
                .and_then(|q| q["text"].as_str())
                .or_else(|| a["text"].as_str())
                .unwrap_or_default();
            let first = first.lines().next().unwrap_or_default();
            if qs.len() > 1 {
                format!("{first}  +{} more", qs.len() - 1)
            } else {
                first.to_string()
            }
        }
        // `escalate {questionId, note?}`: the note says why.
        "escalate" => a["note"]
            .as_str()
            .map(|n| n.lines().next().unwrap_or_default().to_string())
            .unwrap_or_else(|| "question to the user".into()),
        "mcp" => format!(
            "{}.{}",
            a["server"].as_str().unwrap_or("?"),
            a["tool"].as_str().unwrap_or("?")
        ),
        _ => generic_arg(a),
    }
}

/// Any other call at a glance: its action and main string argument, else
/// its scalar arguments as `key=value`, else the compact JSON.
fn generic_arg(a: &Value) -> String {
    const KEYS: [&str; 12] = [
        "path",
        "file_path",
        "url",
        "query",
        "pattern",
        "prompt",
        "text",
        "message",
        "name",
        "symbol",
        "agent",
        "id",
    ];
    let action = a["action"].as_str().filter(|s| !s.is_empty());
    let main = KEYS
        .iter()
        .find_map(|k| a[*k].as_str().filter(|s| !s.trim().is_empty()))
        .map(|v| rel(v.lines().next().unwrap_or_default()));
    match (action, main) {
        (Some(ac), Some(m)) => format!("{ac} {m}"),
        (Some(ac), None) => ac.to_string(),
        (None, Some(m)) => m,
        (None, None) => {
            let Some(obj) = a.as_object() else {
                return String::new();
            };
            if obj.values().all(|v| !v.is_object() && !v.is_array()) {
                obj.iter()
                    .map(|(k, v)| match v.as_str() {
                        Some(s) => format!("{k}={s}"),
                        None => format!("{k}={v}"),
                    })
                    .collect::<Vec<_>>()
                    .join("  ")
            } else {
                a.to_string()
            }
        }
    }
}

/// A write/edit call as a unified diff with path headers (copy diff).
pub(super) fn unified_diff(tool: &Tool) -> Option<String> {
    let path = rel(tool.args["path"].as_str().unwrap_or_default());
    if matches!(tool.name.as_str(), "edit" | "write") && !tool.edits.is_empty() {
        let new_file = tool
            .edits
            .iter()
            .all(|e| e.hunks.iter().all(|h| h.old_text.is_empty()));
        return Some(diff::unified_from_edits(&path, &tool.edits, new_file));
    }
    let (old, new, from) = match tool.name.as_str() {
        "edit" => (
            tool.args["old_string"].as_str().unwrap_or_default(),
            tool.args["new_string"].as_str().unwrap_or_default(),
            format!("a/{path}"),
        ),
        "write" => (
            "",
            tool.args["content"].as_str().unwrap_or_default(),
            "/dev/null".to_string(),
        ),
        _ => return None,
    };
    // Edit fragments rarely end in a newline; avoid "\ No newline" noise.
    let nl = |s: &str| {
        if s.is_empty() || s.ends_with('\n') {
            s.to_string()
        } else {
            format!("{s}\n")
        }
    };
    let (old, new) = (nl(old), nl(new));
    let diff = similar::TextDiff::configure()
        .algorithm(similar::Algorithm::RawMyers)
        .diff_lines(&old, &new);
    Some(
        diff.unified_diff()
            .context_radius(3)
            .header(&from, &format!("b/{path}"))
            .to_string(),
    )
}

/// Variation notes at the head of a tool result (self-evaluated writes,
/// auto-mode `improve`): each as (summary, judge
/// rationale), plus the rest of the output. Candidate numbers are
/// zero-based and 0 is the original call:
/// "self-evaluated 3 candidates; applied variation 2: …" → "self-evaluated 3 · applied #2",
/// "… variation 0: …" → "self-evaluated 3 · kept original",
/// "synthesized from candidates [1,2]: …" → "synthesized from #1+#2".
fn variation_notes(output: &str) -> (Vec<(String, String)>, &str) {
    let mut notes = vec![];
    let mut rest = output;
    loop {
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        if let Some(after) = line.strip_prefix("synthesized from candidates ") {
            let (list, why) = after.split_once(": ").unwrap_or((after, ""));
            let nums: Vec<String> = list
                .trim_matches(['[', ']'])
                .split(',')
                .map(|n| n.trim())
                .filter(|n| !n.is_empty())
                .map(|n| format!("#{n}"))
                .collect();
            notes.push((
                format!("synthesized from {}", nums.join("+")),
                why.trim().to_string(),
            ));
            rest = tail;
            continue;
        }
        let Some((kind, after)) = ["self-evaluated ", "improved "]
            .iter()
            .find_map(|k| line.strip_prefix(k).map(|a| (k.trim(), a)))
        else {
            break;
        };
        let Some((count, after)) = after.split_once(" candidates; ") else {
            break;
        };
        let (how, why) = after.split_once(": ").unwrap_or((after, ""));
        let (why, invalid) = match why.strip_suffix("; synthesis invalid; used best single pick") {
            Some(w) => (w, true),
            None => (why, false),
        };
        let mut how = match how.strip_prefix("applied variation ") {
            Some("0") => "kept original".to_string(),
            Some(k) => format!("applied #{k}"),
            None => how.to_string(),
        };
        if invalid {
            how.push_str(" (synthesis invalid)");
        }
        notes.push((format!("{kind} {count} · {how}"), why.trim().to_string()));
        rest = tail;
    }
    (notes, rest)
}

const TAMPER: &str = "REVIEWER-GUIDANCE TAMPER";

/// A reviewer-guidance tamper flag in a tool result: (a short reading of the flag,
/// the output without it). JSON results carry it as `reviewerGuidanceTamper`.
fn tamper_flag(out: &str) -> (Option<String>, Option<String>) {
    if !out.contains(TAMPER) {
        return (None, None);
    }
    let (flag, rest) = match serde_json::from_str::<Value>(out.trim()) {
        Ok(Value::Object(mut o)) if o.contains_key("reviewerGuidanceTamper") => {
            let f = o
                .remove("reviewerGuidanceTamper")
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            (f, Value::Object(o).to_string())
        }
        _ => {
            let at = out.find(TAMPER).unwrap_or(out.len());
            (
                out[at..].trim().to_string(),
                out[..at].trim_end().to_string(),
            )
        }
    };
    let rest = rest.trim_end().trim_end_matches('[').trim_end().to_string();
    let detail = flag
        .trim_start_matches('[')
        .trim_end_matches(']')
        .strip_prefix(TAMPER)
        .map(|d| d.trim_start_matches(':').trim())
        .unwrap_or(&flag);
    // The usual flag reads short: what changed, what the engine did.
    let short = detail
        .split_once(" changed without a user-approved write; ")
        .map(|(path, after)| {
            let what = after.split(". ").next().unwrap_or(after);
            format!(
                "{} changed without your approval · {what} · approval asked",
                rel(path)
            )
        });
    (
        Some(format!(
            "reviewer guidance: {}",
            short.unwrap_or_else(|| rel(detail))
        )),
        Some(rest),
    )
}

/// A bash result (`[Error: ]exit: N\nstdout:\n…\nstderr:\n…`) for
/// display: stdout, then stderr, then the exit status when it is not 0.
/// The model still gets the labelled original.
fn bash_display(out: &str) -> Option<String> {
    let rest = out.strip_prefix("Error: ").unwrap_or(out);
    let (status, rest) = rest.strip_prefix("exit: ")?.split_once('\n')?;
    let rest = rest.strip_prefix("stdout:\n")?;
    let (stdout, stderr) = match rest.rfind("\nstderr:\n") {
        Some(i) => (&rest[..i], &rest[i + "\nstderr:\n".len()..]),
        None => (rest.strip_suffix("\nstderr:").unwrap_or(rest), ""),
    };
    let mut parts: Vec<String> = vec![];
    for p in [stdout, stderr] {
        let p = p.trim_end_matches('\n');
        if !p.trim().is_empty() {
            parts.push(p.to_string());
        }
    }
    if status != "0" {
        parts.push(format!("exit {status}"));
    }
    Some(parts.join("\n"))
}

/// A script result (`{"stdout": …, "value": …}`) for display: the printed
/// text, then `→ value` when the final expression has one.
fn script_display(out: &str) -> Option<String> {
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    let obj = v.as_object()?;
    // Guidance notices printed by the script show as their own row.
    let stdout = crate::engine::strip_guidance(obj.get("stdout")?.as_str()?.to_string());
    let mut parts: Vec<String> = vec![];
    if !stdout.trim().is_empty() {
        parts.push(stdout.trim_end_matches('\n').to_string());
    }
    match obj.get("value") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => {
            let s = crate::engine::strip_guidance(s.clone());
            parts.push(format!("→ {}", bash_display(&s).unwrap_or(s)))
        }
        Some(v) => parts.push(format!(
            "→ {}",
            serde_json::to_string_pretty(v).unwrap_or_default()
        )),
    }
    Some(parts.join("\n"))
}

fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60)
    }
}

fn fmt_chars(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}k chars", n as f64 / 1000.0)
    } else {
        format!("{n} chars")
    }
}

pub(super) fn render_tool(
    tool: &Tool,
    expanded: bool,
    box_scroll: Option<usize>,
    width: usize,
    tick: usize,
) -> (Vec<Row>, Option<BoxInfo>) {
    let t = theme();
    let reviewing = tool.review.as_ref().is_some_and(|r| r.result.is_none());
    // An out-of-scope write/edit returns a request, not a write.
    let scope = matches!(tool.name.as_str(), "write" | "edit")
        .then_some(tool.output.as_deref())
        .flatten()
        .and_then(scope_result);
    let (icon, color) = match (&tool.output, tool.is_error) {
        (None, _) if reviewing => (SPINNER[tick % SPINNER.len()], t.link),
        (None, _) => (SPINNER[tick % SPINNER.len()], t.yellow),
        _ if scope.as_ref().is_some_and(|s| s.0 != "applied") => {
            match scope.as_ref().unwrap().0.as_str() {
                "rejected" | "failed" => ("✗", t.red),
                _ => ("⇄", t.yellow),
            }
        }
        (Some(_), false) => ("✓", t.green),
        (Some(_), true) => ("✗", t.red),
    };
    let path = tool.args["path"].as_str().unwrap_or_default();
    let diff = match tool.name.as_str() {
        // A finished call: the engine's hunks, with line numbers.
        "edit" | "write" if !tool.edits.is_empty() => Some(diff::render_edits(path, &tool.edits)),
        "edit" => Some(diff::render_file(
            path,
            tool.args["old_string"].as_str().unwrap_or_default(),
            tool.args["new_string"].as_str().unwrap_or_default(),
            None,
        )),
        "write" => Some(diff::render_file(
            path,
            "",
            tool.args["content"].as_str().unwrap_or_default(),
            Some((1, 1)),
        )),
        _ => None,
    };
    let duration = tool.run_time();
    let mut tail = vec![];
    if let Some(r) = &tool.review {
        match &r.result {
            None => tail.push(Span::styled(
                format!("  reviewing with {}…", r.model),
                Style::default().fg(t.link),
            )),
            Some((true, _, _)) => tail.push(Span::styled("  ✓ safe", Style::default().fg(t.green))),
            Some((false, verdict, reason)) => {
                let why = reason.lines().next().unwrap_or_default();
                tail.push(Span::styled(
                    format!("  ✗ {verdict}: {}", truncate(why, 40)),
                    Style::default().fg(t.red),
                ))
            }
        }
    }
    // A tamper flag the engine added to the result shows as its own
    // row; the output shows without it.
    let (tamper, stripped) = tool.output.as_deref().map(tamper_flag).unwrap_or_default();
    let (notes, body) = stripped
        .as_deref()
        .or(tool.output.as_deref())
        .map(variation_notes)
        .unwrap_or_default();
    if let Some(d) = &diff {
        tail.push(Span::styled(
            format!(" +{}", d.added),
            Style::default().fg(t.green),
        ));
        if d.removed > 0 {
            tail.push(Span::styled(
                format!(" -{}", d.removed),
                Style::default().fg(t.red),
            ));
        }
    }
    if let Some((status, owner)) = &scope
        && status != "applied"
    {
        let what = match status.as_str() {
            "pending" | "applying" => format!("requested from {owner} · not applied yet"),
            "granted" => "scope granted · not applied; retry".to_string(),
            "rejected" => format!("rejected by {owner} · not applied"),
            "stale" => "stale · re-read and retry".to_string(),
            "unavailable" => "owner unavailable · not applied".to_string(),
            other => format!("{other} · not applied"),
        };
        tail.push(Span::styled(
            format!("  {what}"),
            Style::default().fg(color),
        ));
    }
    // A follow-up's context choice ("fresh" when the cache is cold).
    let task_context = (tool.name == "task")
        .then_some(tool.output.as_deref())
        .flatten()
        .and_then(|o| serde_json::from_str::<Value>(o).ok())
        .and_then(|v| {
            let c = v["context"].as_str()?.to_string();
            Some((
                c,
                v["contextReason"].as_str().unwrap_or_default().to_string(),
            ))
        });
    if let Some((c, _)) = &task_context {
        tail.push(Span::styled(
            format!("  {c} context"),
            Style::default().fg(t.link),
        ));
    }
    for (summary, _) in &notes {
        tail.push(Span::styled(
            format!("  {summary}"),
            Style::default().fg(t.link),
        ));
    }
    if tool.name == "shell" {
        tail.push(Span::styled(
            "  local · output not sent to the model",
            t.faint(),
        ));
    }
    // A question's time is the user's, not the tool's.
    let asks = matches!(tool.name.as_str(), "question" | "escalate");
    if tool.wait_from.is_some() && tool.duration.is_none() {
        tail.push(Span::styled(
            "  awaiting approval",
            Style::default().fg(t.yellow),
        ));
    } else if asks && tool.output.is_none() && tool.args["blocking"] != false {
        tail.push(Span::styled(
            "  waiting on you",
            Style::default().fg(t.yellow),
        ));
    } else if !tool.replayed && !asks {
        tail.push(Span::styled(
            format!("  {}", fmt_duration(duration)),
            t.faint(),
        ));
    }
    let diff_btn = diff.is_some();
    let btn_w = if diff_btn {
        2 + Btn::Diff.label().width()
    } else {
        0
    };
    let tail_w: usize = tail.iter().map(|s| s.content.width()).sum::<usize>() + btn_w;
    let head = format!("  {icon} ");
    let name = if tool.name == "shell" {
        "! ".to_string()
    } else {
        format!("{} ", tool.name)
    };
    let mut row = vec![
        Span::styled(head.clone(), Style::default().fg(color)),
        Span::styled(name.clone(), t.bold()),
    ];
    let mut used = head.width() + name.width();
    if tool.name == "task" {
        // The child, its live status, then the prompt: `build#1 running · prompt`.
        let (who, status) = match &tool.child {
            Some(c) => (
                format!(
                    "{}{} ",
                    c.label,
                    if c.follow_up { " follow-up" } else { "" }
                ),
                crate::engine::status_text(&c.status),
            ),
            None => (
                format!("{} ", tool.args["agent"].as_str().unwrap_or("agent")),
                if tool.output.is_none() {
                    "starting".into()
                } else {
                    String::new()
                },
            ),
        };
        let status_color = match status.as_str() {
            "done" => t.green,
            "roadblocked" => t.keyword,
            "failed" | "killed" => t.red,
            _ => t.yellow,
        };
        let status = if status.is_empty() {
            String::new()
        } else {
            format!("{status} · ")
        };
        used += who.width() + status.width();
        row.push(Span::styled(
            who,
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ));
        row.push(Span::styled(status, Style::default().fg(status_color)));
    }
    let room = width.saturating_sub(used + tail_w + 1);
    let key = truncate(&key_arg(tool), room);
    used += key.width();
    row.push(Span::styled(key, t.muted()));
    used += tail.iter().map(|s| s.content.width()).sum::<usize>();
    row.extend(tail);
    let mut meta = Meta::header(4);
    if diff_btn {
        row.push(Span::raw("  "));
        let x = used + 2;
        meta.buttons
            .push((x as u16, (x + Btn::Diff.label().width()) as u16, Btn::Diff));
        row.push(Span::styled(Btn::Diff.label(), Style::default().fg(t.dim)));
    }
    let mut out: Vec<Row> = vec![(Line::from(row), meta)];
    let gutter = Span::styled("    │ ", Style::default().fg(t.rule));
    let g = std::slice::from_ref(&gutter);
    // The child's write scope and subscriptions, always visible.
    if tool.name == "task" {
        let list = |k: &str| -> Option<String> {
            let v = tool.args[k].as_array()?;
            Some(if v.is_empty() {
                "none".to_string()
            } else {
                v.iter()
                    .filter_map(|p| p.as_str())
                    .map(rel)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        };
        let mut parts = vec![];
        if let Some(sc) = list("scope") {
            parts.push(format!("writes {sc}"));
        }
        if let Some(sub) = list("subscribe") {
            parts.push(format!("watches {sub}"));
        }
        if !parts.is_empty() {
            let line = Line::from(Span::styled(parts.join(" · "), t.faint()));
            out.extend(wrap_rows(plain(vec![line], 0), width, g, g));
        }
    }
    let more_row = |text: String| -> Row {
        (
            Line::from(vec![gutter.clone(), Span::styled(text, t.faint())]),
            Meta {
                skip: true,
                header: true,
                ..Meta::default()
            },
        )
    };

    // Expanded bash and script rows show the whole command or code,
    // highlighted, above the output.
    let source = match tool.name.as_str() {
        "bash" | "shell" => tool.args["command"].as_str().map(|c| ("bash", c)),
        "script" => tool.args["code"].as_str().map(|c| ("python", c)),
        _ => None,
    };
    // A one-line command the header shows whole is not repeated.
    let source = source.filter(|(_, code)| {
        !matches!(tool.name.as_str(), "bash" | "shell")
            || code.trim().lines().count() > 1
            || key_arg(tool).width() > room
    });
    if expanded && let Some((lang, code)) = source {
        let mut hl = crate::highlight::Highlighter::new(lang);
        let lines: Vec<Line<'static>> = code
            .trim_end()
            .lines()
            .map(|l| Line::from(hl.line(&l.replace('\t', "    "), Style::default())))
            .collect();
        out.extend(wrap_rows(plain(lines, 0), width, g, g));
        out.push((
            Line::from(gutter.clone()),
            Meta {
                skip: true,
                ..Meta::default()
            },
        ));
    }
    if let Some(flag) = &tamper {
        let text = format!("! {flag}");
        let mark = Span::styled("    ", Style::default());
        let mut rows = wrap_rows(
            plain(
                vec![Line::from(Span::styled(
                    text,
                    Style::default().fg(t.yellow).add_modifier(Modifier::BOLD),
                ))],
                0,
            ),
            width,
            std::slice::from_ref(&mark),
            &[Span::raw("      ")],
        );
        if !expanded {
            rows.truncate(2);
        }
        out.extend(rows);
    }
    // The whole output went to a file: its size and an open button.
    if let Some((bytes, lines, path)) = tool.output.as_deref().and_then(full_output) {
        let name = path
            .file_name()
            .map(|n| format!("outputs/{}", n.to_string_lossy()))
            .unwrap_or_else(|| path.display().to_string());
        let head = format!(
            "    ↳ full output · {} · {} line{} · ",
            fmt_bytes(bytes),
            lines,
            if lines == 1 { "" } else { "s" }
        );
        let label = Btn::Output.label();
        let room = width.saturating_sub(head.width() + label.width() + 2);
        let name = truncate(&name, room);
        let x = head.width() + name.width() + 2;
        let mut meta = Meta {
            skip: true,
            ..Meta::default()
        };
        meta.buttons
            .push((x as u16, (x + label.width()) as u16, Btn::Output));
        out.push((
            Line::from(vec![
                Span::styled(head, Style::default().fg(t.fog)),
                Span::styled(name, t.faint()),
                Span::raw("  "),
                Span::styled(label, Style::default().fg(t.link)),
            ]),
            meta,
        ));
    }
    if let Some(reason) = &tool.moved {
        out.push((
            Line::from(Span::styled(
                format!("    ↳ moved to background ({reason})"),
                Style::default().fg(t.fog),
            )),
            Meta {
                skip: true,
                ..Meta::default()
            },
        ));
    }
    if expanded && let Some((c, why)) = task_context.as_ref().filter(|(_, w)| !w.is_empty()) {
        out.extend(wrap_rows(
            plain(
                vec![Line::from(vec![
                    Span::styled(format!("{c} context — "), Style::default().fg(t.link)),
                    Span::styled(why.clone(), Style::default().fg(t.fog)),
                ])],
                0,
            ),
            width,
            g,
            g,
        ));
    }
    if expanded {
        for (summary, why) in notes.iter().filter(|(_, w)| !w.is_empty()) {
            out.extend(wrap_rows(
                plain(
                    vec![Line::from(vec![
                        Span::styled(format!("{summary} — judge: "), Style::default().fg(t.link)),
                        Span::styled(why.clone(), Style::default().fg(t.fog)),
                    ])],
                    0,
                ),
                width,
                g,
                g,
            ));
        }
    }
    if expanded
        && let Some(r) = &tool.review
        && let Some((_, verdict, reason)) = &r.result
    {
        let text = format!("safety review ({}): {verdict} — {}", r.model, reason.trim());
        out.extend(wrap_rows(
            plain(
                vec![Line::from(Span::styled(text, Style::default().fg(t.fog)))],
                0,
            ),
            width,
            g,
            g,
        ));
    }
    if !tool.inner.is_empty() {
        // Script inner calls, newest last; collapsed shows the latest few.
        let n = tool.inner.len();
        let limit = if expanded { n } else { 6 };
        if n > limit {
            out.push(more_row(format!("… {} earlier calls", n - limit)));
        }
        for (k, c) in tool.inner.iter().enumerate().skip(n - limit.min(n)) {
            let (icon, color) = match (&c.output, c.is_error) {
                (Some(_), false) => ("✓", t.green),
                (Some(_), true) => ("✗", t.red),
                (None, _) => (SPINNER[(tick + k) % SPINNER.len()], t.yellow),
            };
            let branch = if k + 1 == n { "└ " } else { "├ " };
            let head = format!("    {branch}");
            let tail = if c.waiting {
                Span::styled("  awaiting approval", Style::default().fg(t.yellow))
            } else {
                let d = c.duration.unwrap_or_else(|| c.started.elapsed());
                Span::styled(format!("  {}", fmt_duration(d)), t.faint())
            };
            let room =
                width.saturating_sub(head.width() + c.name.width() + 4 + tail.content.width());
            out.push((
                Line::from(vec![
                    Span::styled(head, Style::default().fg(t.rule)),
                    Span::styled(format!("{icon} "), Style::default().fg(color)),
                    Span::styled(format!("{} ", c.name), t.bold()),
                    Span::styled(
                        truncate(&call_arg(&c.name, &c.args).replace('\n', " "), room),
                        t.muted(),
                    ),
                    tail,
                ]),
                Meta::gutter(6),
            ));
            // A failed host call says why on the next row.
            if c.is_error
                && let Some(why) = c
                    .output
                    .as_deref()
                    .map(|o| o.strip_prefix("Error: ").unwrap_or(o))
                && let Some(first) = why.lines().find(|l| !l.trim().is_empty())
            {
                let bar = if k + 1 == n { "      " } else { "    │ " };
                let room = width.saturating_sub(bar.width() + 2);
                out.push((
                    Line::from(vec![
                        Span::styled(bar, Style::default().fg(t.rule)),
                        Span::styled(
                            format!("  {}", truncate(first.trim(), room)),
                            Style::default().fg(t.red),
                        ),
                    ]),
                    Meta::gutter(8),
                ));
            }
        }
    }
    if let Some(d) = diff {
        let total = d.lines.len();
        let limit = if expanded { total } else { COLLAPSED_DIFF_ROWS };
        let hang = [gutter.clone(), Span::raw(" ".repeat(d.indent))];
        out.extend(wrap_rows(
            plain(d.lines.into_iter().take(limit).collect(), d.indent),
            width,
            g,
            &hang,
        ));
        if total > limit {
            out.push(more_row(format!(
                "… {} more rows (click to expand)",
                total - limit
            )));
        }
    }
    let mut boxinfo = None;
    if tool.output.is_some() {
        let bash = match tool.name.as_str() {
            "bash" | "shell" => bash_display(body),
            "script" => script_display(body),
            _ => None,
        };
        let output = bash.as_deref().unwrap_or(body);
        // The ✗ and the red already say it failed.
        let output = if tool.is_error {
            output.strip_prefix("Error: ").unwrap_or(output)
        } else {
            output
        };
        let show_output = expanded || tool.is_error;
        if show_output {
            let style = if tool.is_error {
                Style::default().fg(t.red)
            } else {
                Style::default().fg(t.fog)
            };
            let all: Vec<&str> = output.lines().collect();
            let limit = if expanded { MAX_OUTPUT_LINES } else { 3 };
            let lines = all
                .iter()
                .take(limit)
                .map(|l| Line::from(Span::styled(l.replace('\t', "    "), style)))
                .collect();
            let body_w = if expanded {
                width.saturating_sub(1)
            } else {
                width
            };
            let body = wrap_rows(plain(lines, 0), body_w, g, g);
            if expanded {
                boxinfo = boxed(&mut out, body, RESULT_VISIBLE, box_scroll, false, &gutter);
            } else {
                out.extend(body);
            }
            if all.len() > limit {
                out.push(more_row(format!("… {} more lines", all.len() - limit)));
            }
            if all.is_empty() && expanded {
                out.push(more_row("(no output)".into()));
            }
        }
    }
    (out, boxinfo)
}

pub(super) fn render_pending(
    p: &Pending,
    scroll: Option<usize>,
    width: usize,
    tick: usize,
) -> (Vec<Row>, Option<BoxInfo>) {
    let t = theme();
    let name = if p.name.is_empty() { "tool" } else { &p.name };
    if p.abandoned.is_some() {
        let text = format!("  · {name} {} (abandoned)", p.key);
        return (
            vec![(
                Line::from(Span::styled(
                    truncate(&text, width),
                    Style::default()
                        .fg(t.dim)
                        .add_modifier(Modifier::CROSSED_OUT),
                )),
                Meta::gutter(4),
            )],
            None,
        );
    }
    let head = format!("  {} ", SPINNER[tick % SPINNER.len()]);
    let label = format!("writing {name}");
    let size = format!("  {}", fmt_chars(p.args.chars().count()));
    let room = width.saturating_sub(head.width() + label.width() + size.width() + 4);
    let key = if p.key.is_empty() {
        String::new()
    } else {
        format!(" · {}", truncate(&p.key, room))
    };
    let mut out = vec![(
        Line::from(vec![
            Span::styled(head, Style::default().fg(t.yellow)),
            Span::styled(
                label,
                Style::default().fg(t.fog).add_modifier(Modifier::BOLD),
            ),
            Span::styled(key, t.muted()),
            Span::styled(size, t.faint()),
        ]),
        Meta::gutter(4),
    )];
    let mut boxinfo = None;
    if let Some(preview) = p.preview.as_deref().filter(|s| !s.is_empty()) {
        let gutter = Span::styled("    ┊ ", Style::default().fg(t.rule));
        let lines = preview
            .lines()
            .map(|l| Line::from(Span::styled(l.replace('\t', "    "), t.faint())))
            .collect();
        let body = wrap_rows(
            plain(lines, 0),
            width.saturating_sub(1),
            std::slice::from_ref(&gutter),
            std::slice::from_ref(&gutter),
        );
        boxinfo = boxed(&mut out, body, PREVIEW_VISIBLE, scroll, true, &gutter);
    }
    (out, boxinfo)
}

/// The key field of a call being written, parsed tolerantly from partial JSON.
pub(super) fn pending_key(name: &str, args: &str) -> String {
    let keys: &[&str] = match name {
        "read" | "write" | "edit" => &["path", "file_path"],
        "bash" => &["command"],
        "grep" | "glob" => &["pattern"],
        "task" => &["prompt", "continue"],
        "question" => &["text"],
        _ => &[
            "path",
            "file_path",
            "command",
            "pattern",
            "query",
            "url",
            "prompt",
        ],
    };
    let v = partial_field(args, keys).unwrap_or_default();
    rel(v.lines().next().unwrap_or_default())
}

/// The string value of the first of `keys` in a (possibly unterminated)
/// JSON object text, decoded as far as it goes.
pub(super) fn partial_field(json: &str, keys: &[&str]) -> Option<String> {
    for key in keys {
        let pat = format!("\"{key}\"");
        let mut from = 0;
        while let Some(at) = json[from..].find(&pat) {
            let after = from + at + pat.len();
            let rest = json[after..].trim_start();
            if let Some(rest) = rest.strip_prefix(':') {
                let rest = rest.trim_start();
                if let Some(body) = rest.strip_prefix('"') {
                    return Some(decode_partial(body));
                }
                if rest.is_empty() {
                    return Some(String::new());
                }
            } else if rest.is_empty() {
                return Some(String::new());
            }
            from = after;
        }
    }
    None
}

fn decode_partial(body: &str) -> String {
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => {}
                Some('b') | Some('f') => {}
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

/// `full output (N bytes, M lines): <path> — use read/grep on it`
/// in a tool result: (bytes, lines, path).
pub(super) fn full_output(output: &str) -> Option<(u64, u64, std::path::PathBuf)> {
    let at = output.find("full output (")?;
    let rest = &output[at + "full output (".len()..];
    let (bytes, rest) = rest.split_once(" bytes, ")?;
    let (lines, rest) = rest.split_once(" line")?;
    let (_, rest) = rest.split_once("): ")?;
    let path = rest
        .split(" — ")
        .next()?
        .lines()
        .next()?
        .trim()
        .trim_matches('`');
    if path.is_empty() {
        return None;
    }
    Some((
        bytes.trim().parse().ok()?,
        lines.trim().parse().ok()?,
        std::path::PathBuf::from(path),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;

    #[test]
    fn criteria_ids_render_on_character_boundaries() {
        for id in ["界界界", "界界界界界界界界界", "123456789"] {
            let args = serde_json::json!({"action": "done", "id": id});
            let expected = format!("done {}", id.chars().take(8).collect::<String>());
            assert_eq!(call_arg("criteria", &args), expected);
            let mut tr = Transcript::default();
            tr.tool_start("call".into(), "criteria".into(), args);
            let last = tr.len() - 1;
            let rendered = tr[last].render(80, 0);
            assert!(!rendered.lines.is_empty());
        }
    }
}
