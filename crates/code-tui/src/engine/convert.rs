//! Engine records to the plain views: one function per record, and the
//! text clean-up (context tags, guidance notices, injection wrappers) that
//! every place showing engine text applies.
use super::text::{strip_ctx, strip_guidance};
use super::views::{
    ChangeView, GuidanceSel, GuidanceView, JobView, Mode, PruneView, QItem, QKind, QOption,
    QueueClass, Queued, ScopeView, Totals, Usage, fmt_cost,
};
use crate::usage;
use code_config::PermissionMode;
use code_engine::schema::{ErrorKind, JobRecord};
use std::path::Path;

impl Mode {
    pub(super) fn engine(self) -> PermissionMode {
        match self {
            Mode::Manual => PermissionMode::Manual,
            Mode::Auto => PermissionMode::Auto,
            Mode::Unattended => PermissionMode::Unattended,
            Mode::Yolo => PermissionMode::Yolo,
        }
    }
    pub(super) fn from_engine(m: PermissionMode) -> Self {
        match m {
            PermissionMode::Manual => Mode::Manual,
            PermissionMode::Auto => Mode::Auto,
            PermissionMode::Unattended => Mode::Unattended,
            PermissionMode::Yolo => Mode::Yolo,
        }
    }
}

pub(super) fn structured_items(q: &code_engine::schema::QuestionRecord) -> Vec<QItem> {
    q.questions
        .iter()
        .map(|q| QItem {
            id: Some(q.id.clone()),
            text: q.text.clone(),
            multi: q.multi_select,
            kind: match q.kind {
                code_engine::schema::QuestionKind::Text => QKind::Text,
                code_engine::schema::QuestionKind::Choice => QKind::Choice,
                code_engine::schema::QuestionKind::Boolean => QKind::Boolean,
            },
            options: q
                .options
                .iter()
                .map(|o| QOption {
                    label: o.label.clone(),
                    description: o.description.clone(),
                })
                .collect(),
        })
        .collect()
}

pub(super) fn job_view(j: JobRecord) -> JobView {
    JobView {
        id: j.id,
        owner: j.owner,
        kind: j.kind,
        status: j.status,
        command: j.command,
        agent: j.agent_id,
        started: j.started,
        updated: j.updated,
        report: j.report,
    }
}

pub(super) fn scope_view(r: code_engine::scopes::ScopeRequest) -> ScopeView {
    ScopeView {
        id: r.id,
        requester: r.requester,
        owner: r.owner,
        path: r.path,
        tool: r.call.name,
        args: r.call.arguments,
        status: r.status,
        wait: r.wait,
        escalation: r.escalation,
        reason: r.reason,
        output: r.output.map(|o| strip_guidance(strip_ctx(&o))),
    }
}

pub(super) fn change_views(
    changes: Vec<code_engine::subscriptions::SubscriptionChange>,
) -> Vec<ChangeView> {
    changes
        .into_iter()
        .map(|c| ChangeView {
            path: c.path,
            source: c.source,
            text: c.text,
        })
        .collect()
}

pub(super) fn guidance_sel(s: code_engine::guidance_selector::GuidanceSelection) -> GuidanceSel {
    GuidanceSel {
        id: s.id,
        agent: s.agent_id,
        context: s.context,
        model: s.model,
        items: s
            .items
            .into_iter()
            .map(|i| (i.id, i.title, i.text))
            .collect(),
        reason: s.reason,
        path: s.trigger.path,
        language: s.trigger.language,
        area: s.trigger.area,
        first_write: s.trigger.first_write,
        created: s.created,
        error: s.error,
    }
}

pub(super) fn prune_view(r: code_engine::PruneReport) -> PruneView {
    PruneView {
        before: r.tokens_before,
        after: r.tokens_after,
        saved: r.tokens_saved,
        delegations: r.delegations,
        tool_outputs: r.tool_outputs,
        tail_batches: r.tail_batches,
        cache_break: r.cache_break,
    }
}

pub(super) fn guidance_view(r: code_engine::ReviewerGuidanceRequest) -> GuidanceView {
    GuidanceView {
        library: r.library,
        id: r.id,
        path: r.path,
        sha256: r.sha256,
        content: r.content,
        tampered: r.tampered,
        deleted: r.deleted,
    }
}

pub(super) fn usage_view(u: &code_engine::schema::UsageRecord) -> Usage {
    Usage {
        model: u.model.clone(),
        provider: if u.provider.is_empty() {
            u.model.split('/').next().unwrap_or_default().to_string()
        } else {
            u.provider.clone()
        },
        input: u.tokens.input_tokens.unwrap_or(0),
        output: u.tokens.output_tokens.unwrap_or(0),
        cache_read: u.tokens.cache_read_tokens.unwrap_or(0),
        cache_write: u.tokens.cache_write_tokens.unwrap_or(0),
        cost: u.cost_usd,
        quota: u.quota.as_ref().and_then(usage::quota_limit),
    }
}

impl QueueClass {
    pub(super) fn config(self) -> code_config::QueueClass {
        match self {
            Self::NextTurn => code_config::QueueClass::NextTurn,
            Self::Steer => code_config::QueueClass::Steer,
            Self::Interrupt => code_config::QueueClass::Interrupt,
        }
    }
    pub(super) fn from_config(c: code_config::QueueClass) -> Self {
        match c {
            code_config::QueueClass::NextTurn => QueueClass::NextTurn,
            code_config::QueueClass::Steer => QueueClass::Steer,
            code_config::QueueClass::Interrupt => QueueClass::Interrupt,
        }
    }
}

pub(super) fn cost_of(t: &code_engine::schema::SessionTotals) -> String {
    fmt_cost(t.known_cost_usd, t.known_cost_calls, t.unknown_cost_calls)
}

pub(super) fn totals_view(t: &code_engine::schema::SessionTotals) -> Totals {
    Totals {
        calls: t.calls,
        input: t.input_tokens,
        output: t.output_tokens,
        cache_read: t.cache_read_tokens,
        cache_write: t.cache_write_tokens,
        known_cost: t.known_cost_usd,
        known_calls: t.known_cost_calls,
        unknown_calls: t.unknown_cost_calls,
    }
}

pub(super) fn queued_view(item: code_engine::QueuedMessage, assets: &Path) -> Queued {
    Queued {
        id: item.id,
        agent: item.agent,
        text: user_wire(&item.text, &item.attachments, assets),
        class: QueueClass::from_config(item.class),
    }
}

/// Recalled and replayed messages use immutable imported attachment copies.
pub(super) fn user_wire(
    text: &str,
    attachments: &[code_engine::schema::Attachment],
    assets: &Path,
) -> String {
    let mut text = strip_ctx(text);
    for attachment in attachments {
        let path = assets.join(&attachment.path);
        let path = path.to_string_lossy();
        let kind = crate::attach::kind_of(std::path::Path::new(path.as_ref()));
        let id = crate::paste::fresh_id(&path);
        if !text.is_empty() && !text.ends_with(char::is_whitespace) {
            text.push(' ');
        }
        text.push_str(&crate::paste::attach_wire(&id, kind, &path));
    }
    text
}

/// The label an error kind gets in front of its message ("error" gets none).
pub(super) fn error_kind(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Auth => "auth",
        ErrorKind::Network => "network",
        ErrorKind::Billing => "billing",
        ErrorKind::RateLimit => "rate limit",
        ErrorKind::ContextOverflow => "context",
        ErrorKind::BadRequest => "error",
        ErrorKind::Unavailable => "unavailable",
        ErrorKind::LockConflict => "locked",
        ErrorKind::ReadOnly => "read-only",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_questions_keep_engine_ids_kinds_and_options() {
        let q: code_engine::schema::QuestionRecord = serde_json::from_value(serde_json::json!({
            "id":"set", "asker":"root", "recipient":null, "breadcrumb":"root", "text":"summary", "answer":null, "note":null,
            "questions":[{"id":"q1", "text":"Pick", "kind":"choice", "options":[{"label":"A"}], "multiSelect":true},
                {"id":"q2", "text":"Why?", "kind":"text"}]
        })).unwrap();
        let items = structured_items(&q);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id.as_deref(), Some("q1"));
        assert!(matches!(items[0].kind, QKind::Choice));
        assert!(items[0].multi);
        assert_eq!(items[0].options[0].label, "A");
        assert_eq!(items[1].text, "Why?");
    }
}
