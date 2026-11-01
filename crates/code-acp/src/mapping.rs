//! Pure mappings between engine records and ACP v1 payloads: tool-call
//! presentation, permission options (Cockpit's option mapping, extended to the
//! engine's three decisions) and question forms.

use code_engine::schema::{
    ApprovalDecision, ApprovalRequest, QuestionAnswer, QuestionKind, QuestionSpec,
};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Text content blocks are capped so one tool result never nears the frame cap.
const MAX_TEXT: usize = 64 * 1024;

pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn first_line(s: &str, max: usize) -> String {
    clip(s.lines().next().unwrap_or("").trim(), max)
}

pub fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": clip(text, MAX_TEXT)})
}

/// `ToolCallContent::Content` holding text.
pub fn text_content(text: &str) -> Value {
    json!({"type": "content", "content": text_block(text)})
}

pub fn diff_content(path: &Path, old: Option<String>, new: String) -> Value {
    let mut v = json!({"type": "diff", "path": path, "newText": clip(&new, MAX_TEXT)});
    if let Some(old) = old {
        v["oldText"] = json!(clip(&old, MAX_TEXT));
    }
    v
}

/// Old and new sides of a unified diff's hunks (the engine's actual edit diff).
pub fn diff_from_unified(path: &Path, unified: &str) -> Option<Value> {
    let (mut old, mut new, mut hunks) = (String::new(), String::new(), 0);
    for line in unified.lines() {
        if line.starts_with("@@") {
            if hunks > 0 {
                old.push_str("⋮\n");
                new.push_str("⋮\n");
            }
            hunks += 1;
        } else if line.starts_with("---") || line.starts_with("+++") || line.starts_with('\\') {
        } else if let Some(rest) = line.strip_prefix('-') {
            old.push_str(rest);
            old.push('\n');
        } else if let Some(rest) = line.strip_prefix('+') {
            new.push_str(rest);
            new.push('\n');
        } else if hunks > 0 {
            let rest = line.strip_prefix(' ').unwrap_or(line);
            old.push_str(rest);
            old.push('\n');
            new.push_str(rest);
            new.push('\n');
        }
    }
    (hunks > 0).then(|| diff_content(path, Some(old), new))
}

pub fn abs(cwd: &Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        PathBuf::from(home).join(rest)
    } else {
        cwd.join(p)
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

/// ACP `ToolKind` for an engine tool.
pub fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "history" => "read",
        "write" | "edit" => "edit",
        "bash" => "execute",
        "grep" | "glob" => "search",
        "task" | "question" | "criteria" | "message" | "decide" => "think",
        "agents" | "decisions" => "read",
        "web_fetch" | "web_search" => "fetch",
        _ => "other",
    }
}

pub fn tool_kind_with_args(name: &str, args: &Value) -> &'static str {
    if name == "agents" && matches!(args["action"].as_str(), Some("stop" | "finalize")) {
        "think"
    } else {
        tool_kind(name)
    }
}

/// A path as written, shortened to be relative to `cwd` when inside it.
fn show_path<'a>(path: &'a str, cwd: Option<&Path>) -> &'a str {
    cwd.and_then(|c| Path::new(path).strip_prefix(c).ok())
        .and_then(|p| p.to_str())
        .filter(|p| !p.is_empty())
        .unwrap_or(path)
}

pub fn tool_title(name: &str, args: &Value, cwd: Option<&Path>) -> String {
    let path = show_path(arg(args, "path"), cwd);
    match name {
        "read" => format!("Read {path}"),
        "write" => format!("Write {path}"),
        "edit" => format!("Edit {path}"),
        "bash" => {
            let cmd = first_line(arg(args, "command"), 100);
            if args["background"].as_bool() == Some(true) {
                format!("{cmd} (background)")
            } else {
                cmd
            }
        }
        "grep" => format!(
            "grep {:?}{}",
            arg(args, "pattern"),
            if path.is_empty() {
                String::new()
            } else {
                format!(" in {path}")
            }
        ),
        "glob" => format!("glob {}", arg(args, "pattern")),
        "task" => match args.get("continue").and_then(Value::as_str) {
            Some(child) => format!("Continue {child}: {}", first_line(arg(args, "prompt"), 80)),
            None => format!(
                "Task {}: {}",
                args.get("agent").and_then(Value::as_str).unwrap_or("agent"),
                first_line(arg(args, "prompt"), 80)
            ),
        },
        "script" => "Script".into(),
        "question" => "Question".into(),
        "mcp" => format!("MCP {}.{}", arg(args, "server"), arg(args, "tool")),
        other => other.to_string(),
    }
}

pub fn locations(name: &str, args: &Value, cwd: &Path) -> Vec<Value> {
    let path = arg(args, "path");
    if !matches!(name, "read" | "write" | "edit") || path.is_empty() {
        return vec![];
    }
    let mut loc = json!({"path": abs(cwd, path)});
    if let Some(line) = args.get("offset").and_then(Value::as_u64) {
        loc["line"] = json!(line);
    }
    vec![loc]
}

pub fn edit_locations(details: &code_engine::schema::ToolResultDetails) -> Vec<Value> {
    details
        .edits
        .iter()
        .flat_map(|edit| {
            edit.hunks
                .iter()
                .map(|hunk| json!({"path": edit.path, "line": hunk.start_line}))
        })
        .collect()
}

/// Content shown when a call starts: the proposed diff for write/edit, the
/// command for bash, the code for script. `old_file` is the current text of a
/// file about to be replaced by `write`.
pub fn start_content(name: &str, args: &Value, cwd: &Path, old_file: Option<String>) -> Vec<Value> {
    match name {
        "write" => vec![diff_content(
            &abs(cwd, arg(args, "path")),
            old_file,
            arg(args, "content").to_string(),
        )],
        "edit" => vec![diff_content(
            &abs(cwd, arg(args, "path")),
            Some(arg(args, "old_string").to_string()),
            arg(args, "new_string").to_string(),
        )],
        "bash" => vec![text_content(&format!(
            "```sh\n{}\n```",
            arg(args, "command")
        ))],
        "script" => vec![text_content(&format!(
            "```python\n{}\n```",
            arg(args, "code")
        ))],
        "task" => vec![text_content(arg(args, "prompt"))],
        _ => vec![],
    }
}

/// Text shown for a finished call (write/edit keep their diff instead).
pub fn end_text(name: &str, output: &str, is_error: bool) -> Option<String> {
    if matches!(name, "write" | "edit") && !is_error {
        return None;
    }
    if output.trim().is_empty() {
        return None;
    }
    Some(if name == "task" {
        // `{agentId,status,report,...}`: show the report, not the JSON.
        match serde_json::from_str::<Value>(output) {
            Ok(v) if v.get("report").is_some_and(Value::is_string) => format!(
                "**{}** {}\n\n{}",
                v["agentId"].as_str().unwrap_or("subagent"),
                v["status"].as_str().unwrap_or(""),
                v["report"].as_str().unwrap_or("")
            ),
            Ok(v) if v.get("jobId").is_some() => format!(
                "**{}** running in the background (job {})",
                v["agentId"].as_str().unwrap_or("subagent"),
                v["jobId"].as_str().unwrap_or("")
            ),
            _ => output.to_string(),
        }
    } else if name == "question" {
        output.to_string()
    } else {
        format!("```\n{}\n```", output.trim_end())
    })
}

// ---- permissions ---------------------------------------------------------

/// Option ids sent with an approval request.
pub const ALLOW_ONCE: &str = "allow-once";
pub const ALLOW_SESSION: &str = "allow-session";
pub const DENY: &str = "deny";

/// Allow once, allow for this session (only when the engine offers a session
/// grant key; wrappers and compound commands are once-only) and deny.
pub fn approval_options(req: &ApprovalRequest) -> Vec<Value> {
    let mut options =
        vec![json!({"optionId": ALLOW_ONCE, "name": "Allow once", "kind": "allow_once"})];
    if let Some(key) = &req.grant_key {
        options.push(json!({
            "optionId": ALLOW_SESSION,
            "name": format!("Allow for session ({})", clip(key, 60)),
            "kind": "allow_always",
        }));
    }
    options.push(json!({"optionId": DENY, "name": "Deny", "kind": "reject_once"}));
    options
}

/// The engine decision for a `session/request_permission` result.
/// ACP has no feedback field, so a denial carries a fixed explanation.
pub fn approval_decision(result: &Value) -> ApprovalDecision {
    let outcome = &result["outcome"];
    match (outcome["outcome"].as_str(), outcome["optionId"].as_str()) {
        (Some("selected"), Some(ALLOW_ONCE)) => ApprovalDecision::Once,
        (Some("selected"), Some(ALLOW_SESSION)) => ApprovalDecision::Session,
        (Some("selected"), _) => ApprovalDecision::Deny {
            feedback: "denied by the user (ACP client)".into(),
        },
        _ => ApprovalDecision::Deny {
            feedback: "approval cancelled by the ACP client".into(),
        },
    }
}

/// Breadcrumb, job, target, reason and sidecar verdict as Markdown.
pub fn approval_details(req: &ApprovalRequest) -> String {
    let mut lines = vec![format!(
        "**{}** wants to run `{}`",
        req.breadcrumb, req.tool
    )];
    if let Some(job) = &req.job_id {
        lines.push(format!("Job: `{job}`"));
    }
    if let Some(command) = &req.command {
        lines.push(format!("```sh\n{command}\n```"));
    } else if let Some(target) = &req.target {
        lines.push(format!("Target: `{target}`"));
    }
    if !req.reason.is_empty() {
        lines.push(format!("Reason: {}", req.reason));
    }
    if let Some(verdict) = &req.sidecar_verdict {
        let mut s = format!("Safety review: **{verdict}**");
        if let Some(r) = req.sidecar_reason.as_deref().filter(|r| !r.is_empty()) {
            s.push_str(&format!(" — {r}"));
        }
        if let Some(h) = req.sidecar_hint.as_deref().filter(|h| !h.is_empty()) {
            s.push_str(&format!(" (hint: {h})"));
        }
        lines.push(s);
    }
    if let Some(key) = &req.grant_key {
        lines.push(format!("Session grant covers: `{key}`"));
    }
    lines.join("\n\n")
}

/// Permission request content: the actual diff for write/edit, the engine's
/// unified diff otherwise, then the details.
pub fn approval_content(req: &ApprovalRequest, cwd: &Path) -> Vec<Value> {
    let mut content = vec![];
    let path = req.target.as_deref().map(|t| abs(cwd, t)).or_else(|| {
        req.resolved
            .get("path")
            .and_then(Value::as_str)
            .map(|p| abs(cwd, p))
    });
    match (&req.diff, path) {
        (Some(diff), Some(path)) if matches!(req.tool.as_str(), "write" | "edit") => {
            if let Some(d) = diff_from_unified(&path, diff) {
                content.push(d);
            }
        }
        (Some(diff), _) => content.push(text_content(&format!("```diff\n{diff}\n```"))),
        _ => {}
    }
    content.push(text_content(&approval_details(req)));
    content
}

// ---- questions -----------------------------------------------------------

const OTHER: &str = "__other";

/// One form for a whole question set (`elicitation/create`, form mode).
/// Nothing is required: every question can be skipped. A choice or boolean
/// gets a companion free-text "Other" field (D62 `allowOther`).
pub fn elicitation_schema(specs: &[QuestionSpec]) -> Value {
    let mut properties = Map::new();
    for q in specs {
        let title = clip(&q.text, 500);
        let prop = match q.kind {
            QuestionKind::Text => json!({"type": "string", "title": title}),
            QuestionKind::Boolean => json!({"type": "boolean", "title": title}),
            QuestionKind::Choice => {
                let options: Vec<Value> = q
                    .options
                    .iter()
                    .map(|o| {
                        let mut v = json!({"const": o.label, "title": o.label});
                        if let Some(d) = &o.description {
                            v["description"] = json!(d);
                        }
                        v
                    })
                    .collect();
                if q.multi_select {
                    json!({"type": "array", "title": title, "items": {"anyOf": options}})
                } else {
                    json!({"type": "string", "title": title, "oneOf": options})
                }
            }
        };
        properties.insert(q.id.clone(), prop);
        if q.kind != QuestionKind::Text {
            properties.insert(
                format!("{}{OTHER}", q.id),
                json!({"type": "string", "title": "Other (optional)"}),
            );
        }
    }
    json!({"type": "object", "properties": properties, "required": []})
}

/// Answers from an accepted form; empty fields are skipped questions.
pub fn elicitation_answers(specs: &[QuestionSpec], content: &Value) -> Vec<QuestionAnswer> {
    specs
        .iter()
        .map(|q| {
            let mut a = QuestionAnswer {
                id: q.id.clone(),
                ..Default::default()
            };
            let value = &content[&q.id];
            let other = content[format!("{}{OTHER}", q.id)]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            match q.kind {
                QuestionKind::Text => {
                    a.text = value
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(Into::into)
                }
                QuestionKind::Boolean => match value.as_bool() {
                    Some(b) => {
                        a.boolean = Some(b);
                        a.note = other;
                    }
                    None => a.other = other,
                },
                QuestionKind::Choice => {
                    a.labels = match value {
                        Value::String(s) if !s.is_empty() => vec![s.clone()],
                        Value::Array(items) => items
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect(),
                        _ => vec![],
                    };
                    // A single choice is one label or Other.
                    if q.multi_select || a.labels.is_empty() {
                        a.other = other;
                    }
                }
            }
            a.skipped =
                a.text.is_none() && a.boolean.is_none() && a.labels.is_empty() && a.other.is_none();
            a
        })
        .collect()
}

pub const SKIP: &str = "skip";

/// Fallback for clients without form elicitation: one permission prompt per
/// choice or boolean question. `None` for free text, which has no ACP
/// permission equivalent. A multi-select choice can pick one label here.
pub fn question_options(q: &QuestionSpec) -> Option<Vec<Value>> {
    let mut options: Vec<Value> = match q.kind {
        QuestionKind::Text => return None,
        QuestionKind::Boolean => vec![
            json!({"optionId": "yes", "name": "Yes", "kind": "allow_once"}),
            json!({"optionId": "no", "name": "No", "kind": "reject_once"}),
        ],
        QuestionKind::Choice => q
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let name = match &o.description {
                    Some(d) => format!("{} — {}", o.label, clip(d, 80)),
                    None => o.label.clone(),
                };
                json!({"optionId": format!("choice-{i}"), "name": name, "kind": "allow_once"})
            })
            .collect(),
    };
    options.push(json!({"optionId": SKIP, "name": "Skip", "kind": "reject_once"}));
    Some(options)
}

/// The answer for a fallback permission outcome; `None` when the client
/// cancelled (the question stays pending in the engine).
pub fn question_option_answer(q: &QuestionSpec, result: &Value) -> Option<QuestionAnswer> {
    let outcome = &result["outcome"];
    if outcome["outcome"].as_str() != Some("selected") {
        return None;
    }
    let id = outcome["optionId"].as_str().unwrap_or(SKIP);
    let mut a = QuestionAnswer {
        id: q.id.clone(),
        ..Default::default()
    };
    match (id, &q.kind) {
        ("yes", QuestionKind::Boolean) => a.boolean = Some(true),
        ("no", QuestionKind::Boolean) => a.boolean = Some(false),
        (choice, QuestionKind::Choice) => match choice
            .strip_prefix("choice-")
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| q.options.get(i))
        {
            Some(o) => a.labels = vec![o.label.clone()],
            None => a.skipped = true,
        },
        _ => a.skipped = true,
    }
    Some(a)
}

/// Answer for a free-text question the client cannot collect (D62 allows
/// `user unavailable` for every kind).
pub fn unavailable(q: &QuestionSpec) -> QuestionAnswer {
    QuestionAnswer {
        id: q.id.clone(),
        text: Some(
            "user unavailable: this ACP client cannot collect free-text answers; the question was shown to the user, who may answer in their next message".into(),
        ),
        ..Default::default()
    }
}

pub fn answer_summary(a: &QuestionAnswer) -> String {
    if a.skipped {
        return "skipped".into();
    }
    let mut parts = a.labels.clone();
    if let Some(b) = a.boolean {
        parts.push(if b { "yes" } else { "no" }.into());
    }
    if let Some(t) = &a.text {
        parts.push(t.clone());
    }
    if let Some(o) = &a.other {
        parts.push(format!("other: {o}"));
    }
    if let Some(n) = &a.note {
        parts.push(format!("({n})"));
    }
    parts.join(", ")
}
