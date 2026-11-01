//! tui-v13 adapter seams: agent tree rows and control (D131), subtree
//! reporting and wrap-up rows (D132), the decisions ledger (D128), the
//! subagent mode (D133) and the idle-root mailbox (D134).
use super::{Engine, err};
use code_engine::schema::HistoryEvent;
use code_engine::{DecisionRecord, DecisionReview, SubagentMode};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;

/// A durable tree event rendered as a transcript row. The agent whose
/// journal holds it travels beside it ([`super::UiEvent::Tree`]).
#[derive(Debug, Clone)]
pub enum TreeRow {
    /// D132: a child's mid-run report to its parent (replaces the previous
    /// one in place).
    Partial { child: String, text: String },
    /// D132: new descendants, one `- title [id] model=… startedBy=… depth=…`
    /// line each.
    Growth(Vec<String>),
    /// D132: this agent asked `target`'s subtree to wrap up, deepest first.
    FinalizeRequested { target: String, agents: Vec<String> },
    /// D132: this agent is taking its one last turn (tools disabled).
    FinalTurn,
    /// D132: this agent's final report went to its parent.
    Finalized,
    /// D131: this agent stopped `target`'s subtree.
    Stopped {
        target: String,
        reason: String,
        report: String,
    },
    /// D131: a status question to `target`, answered by a hidden fork.
    Asked {
        target: String,
        question: String,
        answer: Option<String>,
        error: Option<String>,
    },
}

/// The tree row a committed history event renders as, if any.
pub(super) fn tree_row(event: &HistoryEvent) -> Option<TreeRow> {
    Some(match event {
        HistoryEvent::AgentPartialReport { agent_id, text } => TreeRow::Partial {
            child: agent_id.clone(),
            text: text.clone(),
        },
        HistoryEvent::SystemReminder { text } => {
            let text = super::summary_text(text);
            let rows = text.strip_prefix("New descendants in your subtree:\n")?;
            TreeRow::Growth(
                rows.lines()
                    .map(|l| l.trim_start_matches("- ").to_string())
                    .filter(|l| !l.is_empty())
                    .collect(),
            )
        }
        HistoryEvent::AgentFinalizeRequested { target, agents } => TreeRow::FinalizeRequested {
            target: target.clone(),
            agents: agents.clone(),
        },
        HistoryEvent::AgentFinalTurn => TreeRow::FinalTurn,
        HistoryEvent::AgentFinalized => TreeRow::Finalized,
        HistoryEvent::AgentStopped {
            target,
            reason,
            report,
        } => TreeRow::Stopped {
            target: target.clone(),
            reason: reason.clone(),
            report: report.clone(),
        },
        HistoryEvent::AgentAsked {
            target,
            question,
            answer,
            error,
        } => TreeRow::Asked {
            target: target.clone(),
            question: question.clone(),
            answer: answer.clone(),
            error: error.clone(),
        },
        _ => return None,
    })
}

/// The D125 suggestion's durable root reminder: (percent, used, window).
pub(super) fn compact_suggestion(event: &HistoryEvent) -> Option<(u64, u64, u64)> {
    let HistoryEvent::SystemReminder { text } = event else {
        return None;
    };
    let text = super::summary_text(text);
    let rest = text.strip_prefix("Root context is at ")?;
    let (pct, rest) = rest.split_once("% (")?;
    let (used, rest) = rest.split_once('/')?;
    let (window, _) = rest.split_once(" tokens)")?;
    Some((pct.parse().ok()?, used.parse().ok()?, window.parse().ok()?))
}

/// One D128 ledger record, in plain frontend types.
#[derive(Debug, Clone)]
pub struct DecisionView {
    pub id: String,
    /// approach, scope, proposal, roadblock, safety, fallback, agent_stop,
    /// agent_finalize.
    pub kind: String,
    pub question: String,
    pub choice: String,
    pub alternatives: Vec<String>,
    pub reason: String,
    pub agent: String,
    pub files: Vec<PathBuf>,
    pub criteria: Vec<String>,
    pub created: String,
    /// open, discussing, kept or changed.
    pub status: String,
    pub review: Option<String>,
}

impl DecisionView {
    pub fn open(&self) -> bool {
        matches!(self.status.as_str(), "open" | "discussing")
    }
}

fn decision_view(r: DecisionRecord) -> DecisionView {
    DecisionView {
        id: r.id,
        kind: r.kind,
        question: r.question,
        choice: r.choice,
        alternatives: r.alternatives,
        reason: r.reason,
        agent: r.deciding_agent,
        files: r.affected_files,
        criteria: r.affected_criteria,
        created: r.created_at,
        status: r.status,
        review: r.review,
    }
}

/// How the user reviews a decision (D128).
pub enum Review {
    Keep,
    Change(String),
    Discuss(Option<String>),
}

/// A finished background agent action: (agent, what, result).
pub enum AgentDone {
    Asked {
        id: String,
        question: String,
        result: Result<String, String>,
    },
    Stopped {
        id: String,
        result: Result<(), String>,
    },
}

impl Engine {
    // ---- D131 ----------------------------------------------------------

    /// `agents list` rows for the `/agents` view (root's perspective).
    pub fn agent_rows(&self, filters: Value) -> Result<Vec<Value>, String> {
        let v = self.session.agent_rows(filters).map_err(err)?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }
    /// Ask a descendant for its status through a hidden fork; the answer
    /// (or error) arrives on `done`. The target is not interrupted or told.
    pub fn ask_agent(&self, id: &str, question: &str, done: std_mpsc::Sender<AgentDone>) {
        let session = self.session.clone();
        let (id, question) = (id.to_string(), question.to_string());
        tokio::spawn(async move {
            let cancel = tokio_util::sync::CancellationToken::new();
            let result = session
                .ask_agent(&id, &question, &cancel)
                .await
                .map(|v| v["answer"].as_str().unwrap_or_default().to_string())
                .map_err(err);
            let _ = done.send(AgentDone::Asked {
                id,
                question,
                result,
            });
        });
    }
    /// Stop a descendant's whole subtree; its parent gets the partial
    /// progress. Completion arrives on `done`.
    pub fn stop_agent(&self, id: &str, reason: &str, done: std_mpsc::Sender<AgentDone>) {
        let session = self.session.clone();
        let (id, reason) = (id.to_string(), reason.to_string());
        tokio::spawn(async move {
            let result = session
                .stop_agent(&id, &reason)
                .await
                .map(|_| ())
                .map_err(err);
            let _ = done.send(AgentDone::Stopped { id, result });
        });
    }
    /// Graceful bottom-up wrap-up of a descendant's subtree (D132): the
    /// agents that will finalize, deepest first.
    pub fn finalize_agent(&self, id: &str) -> Result<Vec<String>, String> {
        let v = self.session.finalize_agent(id).map_err(err)?;
        Ok(v["agents"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    // ---- D128 ----------------------------------------------------------

    /// Every ledger record, newest first.
    pub fn decisions(&self) -> Vec<DecisionView> {
        let mut v: Vec<DecisionView> = self
            .session
            .decisions()
            .into_iter()
            .map(decision_view)
            .collect();
        v.sort_by(|a, b| b.created.cmp(&a.created));
        v
    }
    pub fn decision_open_count(&self) -> usize {
        self.session.decision_open_count()
    }
    /// Keep, change (steering to the deciding agent) or discuss (returns
    /// the side chat's fork id).
    pub fn review_decision(&self, id: &str, review: Review) -> Result<Option<String>, String> {
        let review = match review {
            Review::Keep => DecisionReview::Keep,
            Review::Change(steering) => DecisionReview::Change { steering },
            Review::Discuss(question) => DecisionReview::Discuss { question },
        };
        self.session.review_decision(id, review).map_err(err)
    }

    // ---- D133 ----------------------------------------------------------

    /// The session's subagent mode: (flat, locked).
    pub fn subagents(&self) -> (bool, bool) {
        (
            self.session.subagents() == SubagentMode::Flat,
            self.session.subagents_locked(),
        )
    }
    /// The configured default for new sessions.
    pub fn subagents_default(&self) -> bool {
        self.store.snapshot().config.subagents.mode == SubagentMode::Flat
    }
    pub fn set_subagents(&self, flat: bool) -> Result<(), String> {
        self.session
            .set_subagents(if flat {
                SubagentMode::Flat
            } else {
                SubagentMode::Tree
            })
            .map_err(err)
    }

    // ---- D134 ----------------------------------------------------------

    /// Root mail is waiting and a user cancel holds it until the next
    /// message.
    pub fn mail_held(&self) -> bool {
        self.session.root_mail_suspended() && self.session.has_pending_mail()
    }
}

/// The filters of the `/agents` view as `agents list` arguments.
pub fn agent_filters(
    state: Option<&str>,
    context: Option<&str>,
    subtree: Option<&str>,
    model: Option<&str>,
    query: &str,
) -> Value {
    let mut f = json!({"limit": 100});
    if let Some(s) = state {
        f["state"] = json!(s);
    }
    if let Some(c) = context {
        f["context"] = json!(c);
    }
    if let Some(s) = subtree {
        f["subtree"] = json!(s);
    }
    if let Some(m) = model {
        f["model"] = json!(m);
    }
    if !query.trim().is_empty() {
        f["query"] = json!(query.trim());
    }
    f
}
