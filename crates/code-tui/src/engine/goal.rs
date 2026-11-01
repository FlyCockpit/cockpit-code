//! Plain views of criteria contracts and the `/goal` run (D88–D97, D110),
//! and the editable draft that round-trips through the engine contract.
//! Part of the engine adapter: the rest of the TUI never sees engine types.
use code_engine::{Criterion, CriterionCheck, GoalContract, GoalRun, Proposal, Roadblock};
use serde::{Deserialize, Serialize};

/// The six criteria lists of a contract, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Acceptance,
    Findings,
    Rejection,
    NonGoals,
    AssumedScope,
    Steps,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Section::Acceptance,
        Section::Findings,
        Section::Rejection,
        Section::NonGoals,
        Section::AssumedScope,
        Section::Steps,
    ];
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
    pub fn title(self) -> &'static str {
        match self {
            Section::Acceptance => "Acceptance",
            Section::Findings => "Findings · evidence for each finding or checked-clean",
            Section::Rejection => "Rejection · stop and escalate",
            Section::NonGoals => "Non-goals",
            Section::AssumedScope => "Assumed scope",
            Section::Steps => "Steps",
        }
    }
    /// Singular, for "add …" hints.
    pub fn noun(self) -> &'static str {
        match self {
            Section::Acceptance => "acceptance criterion",
            Section::Findings => "findings area",
            Section::Rejection => "rejection condition",
            Section::NonGoals => "non-goal",
            Section::AssumedScope => "scope assumption",
            Section::Steps => "step",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CriterionView {
    pub id: String,
    pub text: String,
    /// user, parent or self.
    pub owner: String,
    /// open, met, hit or withdrawn.
    pub status: String,
    pub evidence: String,
    pub findings: Vec<FindingView>,
    pub checked_clean: bool,
    /// D132: what the area's review did not verify / did not cover (None:
    /// not stated yet, required before completion).
    pub unverified: Option<Vec<String>>,
    pub not_covered: Option<Vec<String>>,
    /// The check command, if any.
    pub check: Option<String>,
    /// verified, self-reported or check-failed.
    pub verification: String,
    pub override_note: Option<String>,
}

/// One structured finding (D126/D132).
#[derive(Clone, Debug, Default)]
pub struct FindingView {
    /// What is wrong.
    pub text: String,
    pub evidence: String,
    /// critical, high, medium, low or info (empty on legacy rows).
    pub severity: String,
    /// `file:line` references.
    pub locations: Vec<String>,
    pub fix: String,
    /// How it was verified.
    pub verification: String,
    pub confidence: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct ProposalView {
    pub id: String,
    pub criterion_id: String,
    pub change: String,
    /// The replacement check command (engine-o); None removes the old check.
    pub check: Option<String>,
    pub reason: String,
    /// open, accept, edit or reject.
    pub status: String,
    pub resolution: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RoadblockView {
    pub id: String,
    pub criterion_id: Option<String>,
    pub reason: String,
    pub options: Vec<String>,
    pub resolution: Option<String>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ContractView {
    pub objective: String,
    /// Indexed by [`Section::index`].
    pub lists: [Vec<CriterionView>; 6],
    pub proposals: Vec<ProposalView>,
    pub roadblocks: Vec<RoadblockView>,
    pub review: bool,
}

impl ContractView {
    pub fn list(&self, s: Section) -> &[CriterionView] {
        &self.lists[s.index()]
    }
    pub fn is_empty(&self) -> bool {
        self.objective.trim().is_empty() && self.lists.iter().all(|l| l.is_empty())
    }
    /// Acceptance (met, total, verified); withdrawn criteria don't count.
    pub fn counts(&self) -> (usize, usize, usize) {
        let live = self
            .list(Section::Acceptance)
            .iter()
            .chain(self.list(Section::Findings))
            .filter(|c| c.status != "withdrawn");
        let (mut met, mut total, mut verified) = (0, 0, 0);
        for c in live {
            total += 1;
            if c.status == "met" {
                met += 1;
                if c.verification == "verified" {
                    verified += 1;
                }
            }
        }
        (met, total, verified)
    }
    pub fn criterion(&self, id: &str) -> Option<&CriterionView> {
        self.lists.iter().flatten().find(|c| c.id == id)
    }
    pub fn open_proposals(&self) -> impl Iterator<Item = &ProposalView> {
        self.proposals.iter().filter(|p| p.status == "open")
    }
}

/// One lifecycle record of the goal: (at, disposition, phase, reason).
pub type Lifecycle = (String, String, String, Option<String>);

#[derive(Clone, Debug, Default)]
pub struct GoalView {
    /// running, paused, roadblocked, complete or cleared.
    pub disposition: String,
    /// drafting, draft, executing, reviewing, usage-backoff or done.
    pub phase: String,
    pub pause_reason: Option<String>,
    pub contract: ContractView,
    pub model: String,
    pub turns: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    pub unknown_cost_calls: u64,
    pub active_seconds: f64,
    pub no_progress_turns: u64,
    pub no_progress_limit: u64,
    pub review_rounds: u64,
    pub max_review_rounds: u64,
    pub review_panel: bool,
    pub repeated_gap_rounds: u64,
    pub repeated_gap_limit: u64,
    pub usage_resumes: u64,
    pub gaps: Vec<String>,
    pub next_step: Option<String>,
    pub history: Vec<Lifecycle>,
    pub max_usd: Option<f64>,
    pub max_minutes: Option<f64>,
    pub max_tokens: Option<u64>,
}

impl GoalView {
    /// Still part of the session: not cleared (a complete goal stays visible).
    pub fn live(&self) -> bool {
        self.disposition != "cleared"
    }
    pub fn awaiting_approval(&self) -> bool {
        self.phase == "draft" && self.disposition == "paused"
    }
    /// The contract may be edited: paused, roadblocked or a pending draft.
    pub fn editable(&self) -> bool {
        matches!(self.disposition.as_str(), "paused" | "roadblocked") || self.phase == "draft"
    }
    /// A short state word for the status line: "drafting", "running",
    /// "paused (no-progress)", "roadblocked", "complete".
    pub fn state_text(&self) -> String {
        match self.disposition.as_str() {
            "running" => match self.phase.as_str() {
                "executing" => "running".into(),
                "usage-backoff" => "usage backoff".into(),
                p => p.into(),
            },
            "paused" if self.awaiting_approval() => "awaiting approval".into(),
            "paused" => match self.pause_reason.as_deref() {
                Some(r) if !r.is_empty() => format!("paused ({r})"),
                _ => "paused".into(),
            },
            d => d.into(),
        }
    }
}

fn criterion(c: &Criterion) -> CriterionView {
    CriterionView {
        id: c.id.clone(),
        text: c.text.clone(),
        owner: c.owner.clone(),
        status: c.status.clone(),
        evidence: c.evidence.clone(),
        findings: c
            .findings
            .iter()
            .map(|f| FindingView {
                text: f.text.clone(),
                evidence: f.evidence.clone(),
                severity: f.severity.clone(),
                locations: f.locations.clone(),
                fix: f.fix.clone(),
                verification: f.verification.clone(),
                confidence: f.confidence,
            })
            .collect(),
        checked_clean: c.checked_clean,
        unverified: c.unverified.clone(),
        not_covered: c.not_covered.clone(),
        check: c.check.as_ref().map(|k| k.command.clone()),
        verification: c.verification.clone(),
        override_note: c.override_note.clone(),
    }
}

pub(super) fn proposal(p: &Proposal) -> ProposalView {
    ProposalView {
        id: p.id.clone(),
        criterion_id: p.criterion_id.clone(),
        change: p.change.clone(),
        check: p.check.as_ref().map(|k| k.command.clone()),
        reason: p.reason.clone(),
        status: p.status.clone(),
        resolution: p.resolution.clone(),
    }
}

pub(super) fn roadblock(r: &Roadblock) -> RoadblockView {
    RoadblockView {
        id: r.id.clone(),
        criterion_id: r.criterion_id.clone(),
        reason: r.reason.clone(),
        options: r.options.clone(),
        resolution: r.resolution.clone(),
        note: r.note.clone(),
    }
}

pub(super) fn contract(c: &GoalContract) -> ContractView {
    let lists = c.lists().map(|l| l.iter().map(criterion).collect());
    ContractView {
        objective: c.objective.clone(),
        lists,
        proposals: c.proposals.iter().map(proposal).collect(),
        roadblocks: c.roadblocks.iter().map(roadblock).collect(),
        review: c.review,
    }
}

pub(super) fn goal(g: &GoalRun) -> GoalView {
    GoalView {
        disposition: g.disposition.clone(),
        phase: g.phase.clone(),
        pause_reason: g.pause_reason.clone(),
        contract: contract(&g.contract),
        model: g.model.clone(),
        turns: g.turns,
        tokens: g.tokens,
        cost_usd: g.cost_usd,
        unknown_cost_calls: g.unknown_cost_calls,
        active_seconds: g.active_seconds,
        no_progress_turns: g.no_progress_turns,
        no_progress_limit: g.policy.no_progress_turns,
        review_rounds: g.review_rounds,
        max_review_rounds: g.policy.max_review_rounds,
        review_panel: g.policy.review_panel,
        repeated_gap_rounds: g.repeated_gap_rounds,
        repeated_gap_limit: g.policy.repeated_gap_rounds,
        usage_resumes: g.usage_resumes,
        gaps: g.gaps.clone(),
        next_step: g.next_step.clone(),
        history: g
            .history
            .iter()
            .map(|h| {
                (
                    h.at.clone(),
                    h.disposition.clone(),
                    h.phase.clone(),
                    h.reason.clone(),
                )
            })
            .collect(),
        max_usd: g.policy.limits.max_usd,
        max_minutes: g.policy.limits.max_minutes,
        max_tokens: g.policy.limits.max_tokens,
    }
}

/// One editable item: an existing criterion keeps its id (and with it its
/// evidence while text and check are unchanged).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DraftItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// The editable part of a contract: the approval/edit form and the
/// `$EDITOR` YAML document.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractDraft {
    pub objective: String,
    #[serde(default)]
    pub acceptance: Vec<DraftItem>,
    #[serde(default)]
    pub findings: Vec<DraftItem>,
    #[serde(default)]
    pub rejection: Vec<DraftItem>,
    #[serde(default)]
    pub non_goals: Vec<DraftItem>,
    #[serde(default)]
    pub assumed_scope: Vec<DraftItem>,
    #[serde(default)]
    pub steps: Vec<DraftItem>,
}

impl ContractDraft {
    pub fn from_view(c: &ContractView) -> Self {
        let items = |s: Section| {
            c.list(s)
                .iter()
                .filter(|x| x.status != "withdrawn")
                .map(|x| DraftItem {
                    id: Some(x.id.clone()),
                    text: x.text.clone(),
                    check: x.check.clone(),
                })
                .collect()
        };
        Self {
            objective: c.objective.clone(),
            acceptance: items(Section::Acceptance),
            findings: items(Section::Findings),
            rejection: items(Section::Rejection),
            non_goals: items(Section::NonGoals),
            assumed_scope: items(Section::AssumedScope),
            steps: items(Section::Steps),
        }
    }
    pub fn list(&self, s: Section) -> &Vec<DraftItem> {
        match s {
            Section::Acceptance => &self.acceptance,
            Section::Findings => &self.findings,
            Section::Rejection => &self.rejection,
            Section::NonGoals => &self.non_goals,
            Section::AssumedScope => &self.assumed_scope,
            Section::Steps => &self.steps,
        }
    }
    pub fn list_mut(&mut self, s: Section) -> &mut Vec<DraftItem> {
        match s {
            Section::Acceptance => &mut self.acceptance,
            Section::Findings => &mut self.findings,
            Section::Rejection => &mut self.rejection,
            Section::NonGoals => &mut self.non_goals,
            Section::AssumedScope => &mut self.assumed_scope,
            Section::Steps => &mut self.steps,
        }
    }
    /// The YAML document for `$EDITOR`, with a short header.
    pub fn to_yaml(&self) -> String {
        let body = serde_yaml_ng::to_string(self).unwrap_or_default();
        format!(
            "# Goal contract. Edit and save; an empty file or a non-zero editor exit cancels.\n\
             # Keep `id` on items you only reword lightly; a changed text or check reopens the item.\n\
             # findings: investigation areas; evidence per finding or checked-clean is required.\n\
             # check: a shell command whose exit 0 verifies the criterion (optional).\n\
             {body}"
        )
    }
    pub fn from_yaml(text: &str) -> Result<Self, String> {
        let mut d: Self =
            serde_yaml_ng::from_str(text).map_err(|e| format!("contract YAML: {e}"))?;
        for s in Section::ALL {
            let list = d.list_mut(s);
            list.retain(|i| !i.text.trim().is_empty());
            for i in list.iter_mut() {
                i.text = i.text.trim().to_string();
                i.check = i.check.take().filter(|c| !c.trim().is_empty());
            }
        }
        d.objective = d.objective.trim().to_string();
        Ok(d)
    }
    /// What approval needs (the engine enforces it too).
    pub fn problem(&self) -> Option<&'static str> {
        if self.objective.trim().is_empty() {
            Some("the objective is empty")
        } else if self
            .acceptance
            .iter()
            .chain(&self.findings)
            .all(|i| i.text.trim().is_empty())
        {
            Some("add at least one acceptance criterion or findings area")
        } else {
            None
        }
    }
    /// Apply onto `base`: kept ids carry their records (status, evidence,
    /// created), new items get fresh ids; the engine reopens changed ones.
    pub(super) fn apply(&self, base: &GoalContract) -> GoalContract {
        let mut out = base.clone();
        out.objective = self.objective.trim().to_string();
        let build = |items: &[DraftItem], old: &[Criterion]| -> Vec<Criterion> {
            items
                .iter()
                .filter(|i| !i.text.trim().is_empty())
                .map(|i| {
                    let prior =
                        i.id.as_deref()
                            .and_then(|id| old.iter().find(|c| c.id == id));
                    let mut c = prior.cloned().unwrap_or_default();
                    c.text = i.text.trim().to_string();
                    let command = i.check.as_deref().map(str::trim).filter(|s| !s.is_empty());
                    c.check = match (command, &c.check) {
                        (None, _) => None,
                        (Some(cmd), Some(k)) if k.command == cmd => c.check.clone(),
                        (Some(cmd), _) => Some(CriterionCheck {
                            command: cmd.to_string(),
                            cwd: None,
                            timeout_secs: None,
                        }),
                    };
                    c
                })
                .collect()
        };
        let all: Vec<Criterion> = base.criteria().cloned().collect();
        out.acceptance = build(&self.acceptance, &all);
        out.findings = build(&self.findings, &all);
        out.rejection = build(&self.rejection, &all);
        out.non_goals = build(&self.non_goals, &all);
        out.assumed_scope = build(&self.assumed_scope, &all);
        out.steps = build(&self.steps, &all);
        out
    }
}
