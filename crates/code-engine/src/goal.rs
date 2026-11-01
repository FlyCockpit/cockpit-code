//! Per-agent criteria contracts. Snapshots are durable context events, independent of /criteria.
use crate::{Control, State, schema::*, sessions, tools::CommitBarrier};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn owner() -> String {
    "self".into()
}
fn open() -> String {
    "open".into()
}
fn reported() -> String {
    "self-reported".into()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CriterionCheck {
    pub command: String,
    pub cwd: Option<std::path::PathBuf>,
    pub timeout_secs: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Finding {
    pub text: String,
    pub evidence: String,
    pub severity: String,
    pub locations: Vec<String>,
    pub fix: String,
    pub verification: String,
    pub confidence: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Criterion {
    pub id: String,
    pub text: String,
    pub owner: String,
    pub status: String,
    pub evidence: String,
    pub findings: Vec<Finding>,
    pub checked_clean: bool,
    pub unverified: Option<Vec<String>>,
    pub not_covered: Option<Vec<String>>,
    pub check: Option<CriterionCheck>,
    pub verification: String,
    #[serde(rename = "override")]
    pub override_note: Option<String>,
    pub created: String,
    pub updated: String,
}
impl Default for Criterion {
    fn default() -> Self {
        Self {
            id: id(),
            text: String::new(),
            owner: owner(),
            status: open(),
            evidence: String::new(),
            findings: vec![],
            checked_clean: false,
            unverified: None,
            not_covered: None,
            check: None,
            verification: reported(),
            override_note: None,
            created: sessions::now(),
            updated: sessions::now(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    pub id: String,
    pub criterion_id: String,
    pub change: String,
    /// Replacement check. An omitted/null check explicitly removes the old check.
    #[serde(default)]
    pub check: Option<CriterionCheck>,
    pub reason: String,
    pub status: String,
    pub resolution: Option<String>,
    pub created: String,
    pub updated: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roadblock {
    pub id: String,
    pub criterion_id: Option<String>,
    pub reason: String,
    pub options: Vec<String>,
    pub resolution: Option<String>,
    pub note: Option<String>,
    pub created: String,
    pub updated: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GoalContract {
    pub objective: String,
    pub acceptance: Vec<Criterion>,
    pub findings: Vec<Criterion>,
    pub rejection: Vec<Criterion>,
    pub non_goals: Vec<Criterion>,
    pub assumed_scope: Vec<Criterion>,
    pub steps: Vec<Criterion>,
    pub proposals: Vec<Proposal>,
    pub roadblocks: Vec<Roadblock>,
    pub review: bool,
    pub revision: u64,
}
fn bounded(s: &str, required: bool) -> Result<(), EngineError> {
    if (required && s.trim().is_empty()) || s.chars().count() > 4096 {
        return Err(EngineError::bad(
            "contract text must contain 1–4096 characters (optional evidence may be empty)",
        ));
    }
    Ok(())
}
impl Finding {
    pub(crate) fn validate(&self) -> Result<(), EngineError> {
        for (name, value) in [
            ("text", &self.text),
            ("evidence", &self.evidence),
            ("fix", &self.fix),
            ("verification", &self.verification),
        ] {
            if value.trim().is_empty() || value.chars().count() > 4096 {
                return Err(EngineError::bad(format!(
                    "finding {name} must contain 1–4096 characters"
                )));
            }
        }
        if !["critical", "high", "medium", "low", "info"].contains(&self.severity.as_str()) {
            return Err(EngineError::bad(
                "finding severity must be critical, high, medium, low or info",
            ));
        }
        if self
            .confidence
            .is_none_or(|n| !n.is_finite() || !(0.0..=1.0).contains(&n))
        {
            return Err(EngineError::bad(
                "finding confidence must be a number from 0 to 1",
            ));
        }
        let location = regex::Regex::new(r"^[^:\r\n]+:[1-9][0-9]*$").unwrap();
        if self.locations.is_empty()
            || self.locations.len() > 32
            || self
                .locations
                .iter()
                .any(|value| value.len() > 4096 || !location.is_match(value))
        {
            return Err(EngineError::bad(
                "finding locations require 1–32 file:line entries (positive line)",
            ));
        }
        Ok(())
    }
}
impl Criterion {
    pub(crate) fn validate_findings(&self) -> Result<(), EngineError> {
        for (name, list) in [
            ("unverified", &self.unverified),
            ("notCovered", &self.not_covered),
        ] {
            let list = list.as_ref().ok_or_else(|| {
                EngineError::bad(format!(
                    "findings met requires explicit {name}:[] (or a list of gaps)"
                ))
            })?;
            if list.len() > 32 {
                return Err(EngineError::bad("findings coverage lists are capped at 32"));
            }
            for item in list {
                bounded(item, true)?;
            }
        }
        for finding in &self.findings {
            finding.validate()?;
        }
        Ok(())
    }
}
fn coverage(c: &mut Criterion, args: &Value) -> Result<(), EngineError> {
    for (name, target) in [
        ("unverified", &mut c.unverified),
        ("notCovered", &mut c.not_covered),
    ] {
        if let Some(value) = args.get(name) {
            *target = Some(
                serde_json::from_value(value.clone())
                    .map_err(|e| EngineError::bad(e.to_string()))?,
            );
        }
    }
    Ok(())
}
impl GoalContract {
    pub fn lists(&self) -> [&Vec<Criterion>; 6] {
        [
            &self.acceptance,
            &self.findings,
            &self.rejection,
            &self.non_goals,
            &self.assumed_scope,
            &self.steps,
        ]
    }
    pub fn criteria(&self) -> impl Iterator<Item = &Criterion> {
        self.lists().into_iter().flatten()
    }
    pub fn criterion_mut(&mut self, id: &str) -> Result<&mut Criterion, EngineError> {
        self.acceptance
            .iter_mut()
            .chain(&mut self.findings)
            .chain(&mut self.rejection)
            .chain(&mut self.non_goals)
            .chain(&mut self.assumed_scope)
            .chain(&mut self.steps)
            .find(|c| c.id == id)
            .ok_or_else(|| EngineError::bad("criterion not found"))
    }
    pub fn validate(&self) -> Result<(), EngineError> {
        self.validate_with_previous(None)
    }
    fn validate_with_previous(&self, previous: Option<&Self>) -> Result<(), EngineError> {
        bounded(&self.objective, false)?;
        let mut ids = std::collections::BTreeSet::new();
        for list in self.lists() {
            if list.len() > 32 {
                return Err(EngineError::bad("contract lists are capped at 32 items"));
            }
            for c in list {
                bounded(&c.text, true)?;
                bounded(&c.evidence, false)?;
                let findings = self.findings.iter().any(|item| item.id == c.id);
                let acceptance = self.completion_criteria().any(|item| item.id == c.id);
                if !findings && (!c.findings.is_empty() || c.checked_clean) {
                    return Err(EngineError::bad(
                        "finding records belong only to findings criteria",
                    ));
                }
                if c.findings.len() > 32 || (c.checked_clean && !c.findings.is_empty()) {
                    return Err(EngineError::bad(
                        "findings are capped at 32; checked-clean cannot coexist with findings",
                    ));
                }
                let mut texts = std::collections::BTreeSet::new();
                for finding in &c.findings {
                    bounded(&finding.text, true)?;
                    bounded(&finding.evidence, false)?;
                    if !texts.insert(&finding.text) {
                        return Err(EngineError::bad("duplicate finding text"));
                    }
                }
                if findings && c.status == "met" && c.override_note.is_none() {
                    let unchanged = previous
                        .and_then(|old| old.findings.iter().find(|old| old.id == c.id))
                        .is_some_and(|old| {
                            serde_json::to_value(old).ok() == serde_json::to_value(c).ok()
                        });
                    if !unchanged {
                        c.validate_findings()?;
                    }
                    if c.findings.is_empty() && !c.checked_clean {
                        return Err(EngineError::bad(
                            "findings met requires recorded findings or checkedClean:true, with evidence",
                        ));
                    }
                    for finding in &c.findings {
                        if finding.evidence.trim().is_empty() {
                            return Err(EngineError::bad(format!(
                                "finding requires evidence: {}",
                                finding.text
                            )));
                        }
                        bounded(&finding.evidence, true)?;
                    }
                }
                if c.status == "hit" || (acceptance && c.status == "met") {
                    bounded(&c.evidence, true)?;
                }
                if c.verification == "verified" && c.check.is_none() {
                    return Err(EngineError::bad("verified criteria require a check"));
                }
                if acceptance
                    && c.status == "met"
                    && c.check.is_some()
                    && c.verification != "verified"
                    && c.override_note.is_none()
                {
                    return Err(EngineError::bad(
                        "criteria with checks may be met only after successful verification",
                    ));
                }
                if c.id.is_empty() || !ids.insert(&c.id) {
                    return Err(EngineError::bad(
                        "criterion IDs must be nonempty and unique",
                    ));
                }
                if !["self", "parent", "user"].contains(&c.owner.as_str())
                    || !["open", "met", "hit", "withdrawn"].contains(&c.status.as_str())
                    || !["verified", "self-reported", "check-failed"]
                        .contains(&c.verification.as_str())
                {
                    return Err(EngineError::bad(
                        "invalid criterion owner, status or verification",
                    ));
                }
                if let Some(check) = &c.check {
                    bounded(&check.command, true)?;
                    if check.timeout_secs.is_some_and(|n| n == 0 || n > 600) {
                        return Err(EngineError::bad("check timeoutSecs must be 1–600"));
                    }
                }
                if let Some(note) = &c.override_note {
                    bounded(note, true)?;
                }
            }
        }
        if self.proposals.len() > 32 || self.roadblocks.len() > 32 {
            return Err(EngineError::bad(
                "proposals and roadblocks are capped at 32 items",
            ));
        }
        for p in &self.proposals {
            bounded(&p.change, true)?;
            bounded(&p.reason, true)?;
            if let Some(check) = &p.check {
                bounded(&check.command, true)?;
                if check.timeout_secs.is_some_and(|n| n == 0 || n > 600) {
                    return Err(EngineError::bad("check timeoutSecs must be 1–600"));
                }
            }
            if let Some(note) = &p.resolution {
                bounded(note, false)?;
            }
        }
        for r in &self.roadblocks {
            bounded(&r.reason, true)?;
            if r.criterion_id.as_ref().is_some_and(|id| !ids.contains(id)) {
                return Err(EngineError::bad("roadblock criterion not found"));
            }
            if let Some(note) = &r.note {
                bounded(note, false)?;
            }
            if r.options.len() > 32 {
                return Err(EngineError::bad("roadblock options capped at 32"));
            }
            for o in &r.options {
                bounded(o, true)?;
            }
        }
        Ok(())
    }
    pub(crate) fn progress_key(&self) -> String {
        let mut c = self.clone();
        c.revision = 0;
        for c in c
            .acceptance
            .iter_mut()
            .chain(&mut c.findings)
            .chain(&mut c.rejection)
            .chain(&mut c.non_goals)
            .chain(&mut c.assumed_scope)
            .chain(&mut c.steps)
        {
            c.created.clear();
            c.updated.clear();
        }
        serde_json::to_string(&c).unwrap()
    }
    pub fn completion_criteria(&self) -> impl Iterator<Item = &Criterion> {
        self.acceptance.iter().chain(&self.findings)
    }
    /// Acceptance and findings both define completion for D96 review.
    pub fn has_completion_criteria(&self) -> bool {
        self.completion_criteria().any(|c| c.status != "withdrawn")
    }
    pub fn complete(&self) -> bool {
        self.completion_criteria().next().is_some()
            && self
                .completion_criteria()
                .all(|c| c.status == "met" || c.status == "withdrawn" || c.override_note.is_some())
    }
    pub fn roadblock(&self) -> Option<&Roadblock> {
        self.roadblocks.iter().find(|r| r.resolution.is_none())
    }
    pub fn counts(&self) -> Value {
        json!({"met":self.completion_criteria().filter(|c|c.status=="met").count(),"total":self.completion_criteria().filter(|c|c.status!="withdrawn").count(),"verified":self.completion_criteria().filter(|c|c.verification=="verified" && c.status=="met").count()})
    }
    pub fn open_context(&self) -> Value {
        json!({"objective":self.objective,"openCriteria":self.criteria().filter(|c|c.status=="open" || c.status=="hit").collect::<Vec<_>>()})
    }
    pub fn own(&mut self, owner: &str) {
        for c in self
            .acceptance
            .iter_mut()
            .chain(&mut self.findings)
            .chain(&mut self.rejection)
            .chain(&mut self.non_goals)
            .chain(&mut self.assumed_scope)
        {
            c.owner = owner.into();
        }
    }
    pub fn report(&self) -> String {
        if self.criteria().next().is_none() && self.roadblock().is_none() {
            return String::new();
        }
        let mut out = format!(
            "\n\nContract: {}\n\n| Kind | Criterion | Owner | Status | Verification | Evidence |\n|---|---|---|---|---|---|\n",
            self.objective.replace('|', "\\|")
        );
        for (kind, list) in [
            "acceptance",
            "findings",
            "rejection",
            "nonGoal",
            "assumedScope",
            "step",
        ]
        .into_iter()
        .zip(self.lists())
        {
            for c in list {
                out.push_str(&format!(
                    "| {kind} | {} ({}) | {} | {} | {} | {} |\n",
                    c.text.replace('|', "\\|"),
                    c.id,
                    c.owner,
                    c.status,
                    c.verification,
                    format!(
                        "{}{}{}",
                        c.evidence,
                        if c.checked_clean {
                            " [checked-clean]"
                        } else {
                            ""
                        },
                        c.findings
                            .iter()
                            .map(|f| format!("; {}", serde_json::to_string(f).unwrap()))
                            .collect::<String>()
                    )
                    .replace('|', "\\|")
                    .replace('\n', " ")
                ));
            }
        }
        for c in &self.findings {
            out.push_str(&format!(
                "\nFindings coverage {}: unverified={}, notCovered={}\n",
                c.id,
                serde_json::to_string(&c.unverified).unwrap(),
                serde_json::to_string(&c.not_covered).unwrap()
            ));
        }
        if let Some(r) = self.roadblock() {
            out.push_str(&format!("\nRoadblock {}: {}\n", r.id, r.reason));
        }
        out
    }
    pub(crate) fn apply_task(&mut self, args: &Value) -> Result<(), EngineError> {
        let previous = self.clone();
        for (key, list) in [
            ("acceptance", &mut self.acceptance),
            ("findings", &mut self.findings),
            ("rejection", &mut self.rejection),
            ("nonGoals", &mut self.non_goals),
            ("assumedScope", &mut self.assumed_scope),
        ] {
            if let Some(items) = args.get(key) {
                let items = items
                    .as_array()
                    .ok_or_else(|| EngineError::bad(format!("{key} must be a list")))?;
                let previous = list.clone();
                *list = items
                    .iter()
                    .map(|v| {
                        let mut c: Criterion = if let Some(text) = v.as_str() {
                            Criterion {
                                text: text.into(),
                                ..Criterion::default()
                            }
                        } else {
                            serde_json::from_value(v.clone())
                                .map_err(|e| EngineError::bad(e.to_string()))?
                        };
                        if let Some(old) = previous.iter().find(|old| {
                            old.id == c.id && old.text == c.text && old.check == c.check
                        }) {
                            c = old.clone();
                        } else {
                            c.status = "open".into();
                            c.evidence.clear();
                            c.findings.clear();
                            c.unverified = None;
                            c.not_covered = None;
                            c.checked_clean = false;
                            c.verification = reported();
                            c.override_note = None;
                            c.created = sessions::now();
                            c.updated = sessions::now();
                        }
                        c.owner = "parent".into();
                        Ok(c)
                    })
                    .collect::<Result<_, EngineError>>()?;
            }
        }
        if let Some(review) = args["review"].as_bool() {
            self.review = review;
        }
        self.validate_with_previous(Some(&previous))
    }
}
fn finding_schema() -> Value {
    json!({"type":"object","properties":{
        "text":{"type":"string"},"evidence":{"type":"string"},
        "severity":{"type":"string","enum":["critical","high","medium","low","info"]},
        "locations":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"string"}},
        "fix":{"type":"string"},"verification":{"type":"string"},"confidence":{"type":"number","minimum":0,"maximum":1}
    },"required":["text","evidence","severity","locations","fix","verification","confidence"],"additionalProperties":false})
}
fn finding_arguments(args: &Value) -> Result<Finding, EngineError> {
    let mut required = vec!["id"];
    let schema = finding_schema();
    required.extend(
        schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str),
    );
    let missing = required
        .into_iter()
        .filter(|key| args.get(*key).is_none_or(Value::is_null))
        .collect::<Vec<_>>();
    let example = r#"{"action":"finding","id":"<findings criterion id>","text":"Unchecked path","evidence":"src/lib.rs:1 inspection","severity":"low","locations":["src/lib.rs:1"],"fix":"Validate the path","verification":"Inspection only; runtime check pending","confidence":0.8}"#;
    let help = |reason: String| {
        EngineError::bad(format!(
            "criteria finding: {reason}. Minimal valid example (replace with actual findings and evidence): {example}"
        ))
    };
    if !missing.is_empty() {
        return Err(help(format!("missing fields: {}", missing.join(", "))));
    }
    let finding: Finding = serde_json::from_value(args.clone()).map_err(|e| help(e.to_string()))?;
    finding.validate().map_err(|e| help(e.message))?;
    Ok(finding)
}
pub(crate) fn definition() -> crate::tools::ToolDefinition {
    crate::tools::ToolDefinition { name:"criteria".into(), description:"Track a durable contract for multi-step work/proof; trivial work just does the task. Children create criteria only when the parent passed them. Derive criteria from user intent; clarify material ambiguity with the owner. finding records evidence, met completes an existing acceptance/findings criterion, step updates a checklist. Acceptance checks execute through permissions when met; findings need no command. Propose changes to user/parent-owned criteria. A rejection hit/roadblock stops and escalates. query reads tree contracts; resolve_proposal/resolve_roadblock can target direct children. Monty: criteria.show(), criteria.add(...); results are dicts. For ordinary progress use final text or long-running child report, not criteria ceremony.".into(),
        schema:json!({"type":"object","properties":{
            "action":{"type":"string","enum":["show","set_objective","add","finding","met","hit","propose","roadblock","step","query","resolve_proposal","resolve_roadblock"]},
            "kind":{"type":"string","enum":["acceptance","findings","rejection","nonGoal","assumedScope","step"]},
            "objective":{"type":"string"},"id":{"type":"string","description":r#"Existing criterion ID required for finding/met/hit/propose. Record one finding: {"action":"finding","id":"F1","text":"Zero divisor","evidence":"math.py:2 inspection","severity":"medium","locations":["math.py:2"],"fix":"Guard zero","verification":"Inspected only","confidence":0.8}. Use actual evidence."#},"text":{"type":"string","description":"Required for add/finding; finding states what is wrong."},"evidence":{"type":"string","description":r#"Required proof for finding/met/hit. After recording F1's findings, complete it: {"action":"met","id":"F1","evidence":"All paths inspected","unverified":[],"notCovered":[]}. List real coverage gaps instead of [] when present."#},
            "findings":{"type":"array","maxItems":32,"items":finding_schema(),"description":"For met: merge full findings, or record them first with finding. Recording alone does not complete a criterion."},"checkedClean":{"type":"boolean","description":"For findings met with no findings: true plus evidence, unverified and notCovered is required."},
            "severity":{"type":"string","enum":["critical","high","medium","low","info"],"description":"Required for finding."},"locations":{"type":"array","minItems":1,"maxItems":32,"description":"Required for finding: one or more file:line locations.","items":{"type":"string","description":"file:line with a positive line number"}},"fix":{"type":"string","description":"Required for finding: suggested correction."},"verification":{"type":"string","description":"Required for finding: how checked; say when inspection only."},"confidence":{"type":"number","minimum":0,"maximum":1,"description":"Required for finding: confidence from 0 to 1."},
            "unverified":{"type":"array","maxItems":32,"items":{"type":"string"},"description":"Required explicit list for findings completion; [] means nothing remains unverified."},"notCovered":{"type":"array","maxItems":32,"items":{"type":"string"},"description":"Required explicit list of excluded areas; [] means none."},
            "check":{"type":["object","null"],"description":"For propose, the replacement check; omitted/null removes the old check when accepted.","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeoutSecs":{"type":"integer","minimum":1,"maximum":600}},"required":["command"],"additionalProperties":false},
            "change":{"type":"string"},"reason":{"type":"string"},"options":{"type":"array","items":{"type":"string"}},
            "status":{"type":["string","array"],"items":{"type":"string"}},"agentId":{"type":"string"},"proposalId":{"type":"string"},"roadblockId":{"type":"string"},
            "resolution":{"type":"string"},"note":{"type":"string"},"scope":{"type":"string"},"depth":{"type":"integer","minimum":0},"agent":{"type":"string"},"sort":{"type":"string","enum":["updated","created","depth"]},"limit":{"type":"integer","minimum":1}
        },"required":["action"],"additionalProperties":false}) }
}
impl Control {
    pub(crate) fn contract(&self, agent: &str) -> GoalContract {
        self.journal
            .lock()
            .unwrap()
            .contracts
            .get(agent)
            .cloned()
            .unwrap_or_default()
    }
    pub(crate) fn save_contract(
        &self,
        agent: &str,
        mut contract: GoalContract,
    ) -> Result<(), EngineError> {
        let (envelope, progress) = {
            let mut j = self.journal.lock().unwrap();
            contract.validate_with_previous(j.contracts.get(agent))?;
            let progress = j
                .contracts
                .get(agent)
                .is_none_or(|old| old.progress_key() != contract.progress_key());
            let revision = j.contracts.get(agent).map_or(0, |c| c.revision);
            if revision != contract.revision {
                return Err(EngineError::bad(
                    "contract changed while this action was running; read criteria.show() and retry",
                ));
            }
            contract.revision = revision.saturating_add(1);
            (j.append(agent, HistoryEvent::Goal { contract })?, progress)
        };
        if progress {
            self.goal_progress
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.emit(EngineEvent::Committed { envelope });
        if agent == "root"
            && let Some(run) = self.goal_state()
        {
            self.emit(EngineEvent::GoalUpdate {
                state: Box::new(run),
            });
        }
        self.changed.notify_waiters();
        Ok(())
    }
    pub(crate) fn resolve_proposal(
        &self,
        agent: &str,
        proposal_id: &str,
        resolution: &str,
        text: Option<String>,
    ) -> Result<(), EngineError> {
        let mut contract = self.contract(agent);
        let proposal = contract
            .proposals
            .iter_mut()
            .find(|p| p.id == proposal_id && p.status == "open")
            .ok_or_else(|| EngineError::bad("open proposal not found"))?;
        if !["accept", "edit", "reject"].contains(&resolution) {
            return Err(EngineError::bad(
                "resolution must be accept, edit or reject",
            ));
        }
        let (criterion, change, check) = (
            proposal.criterion_id.clone(),
            proposal.change.clone(),
            proposal.check.clone(),
        );
        proposal.status = resolution.into();
        proposal.resolution = text.clone();
        proposal.updated = sessions::now();
        if resolution != "reject" {
            let c = contract.criterion_mut(&criterion)?;
            c.text = if resolution == "edit" {
                text.ok_or_else(|| EngineError::bad("edit requires text"))?
            } else {
                change
            };
            c.status = "open".into();
            c.check = check;
            c.evidence.clear();
            c.findings.clear();
            c.unverified = None;
            c.not_covered = None;
            c.checked_clean = false;
            c.verification = reported();
            c.override_note = None;
            c.updated = sessions::now();
        }
        self.save_contract(agent, contract)?;
        self.commit(agent,HistoryEvent::SystemReminder{text:format!("Proposal {proposal_id} resolved: {resolution}. Read criteria.show() for the updated contract.")})?;
        Ok(())
    }
    pub(crate) fn resolve_roadblock(
        &self,
        agent: &str,
        roadblock_id: &str,
        resolution: &str,
        note: Option<String>,
        edits: Option<GoalContract>,
    ) -> Result<(), EngineError> {
        if !["pivot", "stop", "override"].contains(&resolution) {
            return Err(EngineError::bad(
                "resolution must be pivot, stop or override",
            ));
        }
        if matches!(resolution, "pivot" | "override")
            && note.as_deref().is_none_or(|n| n.trim().is_empty())
            && edits.is_none()
        {
            return Err(EngineError::bad(
                "pivot requires text or contract edits; override requires a note",
            ));
        }
        if resolution == "override" {
            bounded(note.as_deref().unwrap_or(""), true)?;
        }
        let mut contract = self.contract(agent);
        let r = contract
            .roadblocks
            .iter_mut()
            .find(|r| r.id == roadblock_id && r.resolution.is_none())
            .ok_or_else(|| EngineError::bad("open roadblock not found"))?;
        r.resolution = Some(resolution.into());
        r.note = note.clone();
        r.updated = sessions::now();
        let criterion = r.criterion_id.clone();
        if let Some(mut edited) = edits {
            edited.revision = contract.revision;
            edited.roadblocks = contract.roadblocks;
            edited.proposals = contract.proposals;
            contract = edited;
        }
        if resolution == "override"
            && let Some(id) = criterion
        {
            let rejection = contract.rejection.iter().any(|c| c.id == id);
            let c = contract.criterion_mut(&id)?;
            c.override_note = note.clone();
            c.status = if rejection { "withdrawn" } else { "met" }.into();
            c.evidence = note.clone().unwrap_or_default();
            c.verification = reported();
            c.updated = sessions::now();
        }
        self.save_contract(agent, contract)?;
        self.commit(
            agent,
            HistoryEvent::SystemReminder {
                text: format!(
                    "Roadblock {roadblock_id}: {resolution}. {}",
                    note.unwrap_or_default()
                ),
            },
        )?;
        Ok(())
    }
}
fn field<'a>(args: &'a Value, key: &str) -> Result<&'a str, EngineError> {
    args[key]
        .as_str()
        .ok_or_else(|| EngineError::bad(format!("{key} required")))
}
fn merge_finding(c: &mut Criterion, finding: Finding) {
    c.checked_clean = false;
    if let Some(old) = c.findings.iter_mut().find(|f| f.text == finding.text) {
        *old = finding;
    } else {
        c.findings.push(finding);
    }
}
pub(crate) async fn execute(
    state: &State,
    args: &Value,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
    tx: &crate::model::EventSender,
) -> Result<String, EngineError> {
    if cancel.is_cancelled() {
        return Err(EngineError::cancelled());
    }
    let action = field(args, "action")?;
    if action == "query" {
        return query(state, args);
    }
    let target = args["agentId"].as_str().unwrap_or(&state.agent_id);
    if target != state.agent_id
        && !["show", "resolve_proposal", "resolve_roadblock"].contains(&action)
    {
        return Err(EngineError::bad(
            "agents may write only their own contract; use propose for owned criteria",
        ));
    }
    if ["resolve_proposal", "resolve_roadblock"].contains(&action) {
        let parent = state
            .control
            .journal
            .lock()
            .unwrap()
            .agents
            .get(target)
            .and_then(|a| a.parent_id.clone());
        if parent.as_deref() != Some(&state.agent_id) {
            return Err(EngineError::bad(
                "only a direct parent may resolve a child's proposal or roadblock",
            ));
        }
        let before = state.control.contract(target);
        barrier
            .commit(|| {
                if action == "resolve_proposal" {
                    state.control.resolve_proposal(
                        target,
                        field(args, "proposalId")?,
                        field(args, "resolution")?,
                        args["text"].as_str().map(str::to_owned),
                    )?;
                } else {
                    state.control.resolve_roadblock(
                        target,
                        field(args, "roadblockId")?,
                        field(args, "resolution")?,
                        args["note"].as_str().map(str::to_owned),
                        None,
                    )?;
                }
                state.control.record_contract_resolutions(
                    &state.agent_id,
                    target,
                    &before,
                    &state.control.contract(target),
                )?;
                Ok(())
            })
            .map_err(|e| EngineError::bad(e.to_string()))?;
        return Ok(serde_json::to_string(&state.control.contract(target)).unwrap());
    }
    let mut contract = state.control.contract(target);
    if action == "show" {
        return Ok(serde_json::to_string(&contract).unwrap());
    }
    if matches!(action, "add" | "set_objective")
        && state
            .control
            .journal
            .lock()
            .unwrap()
            .agents
            .get(target)
            .is_some_and(|agent| agent.parent_id.is_some())
        && !contract
            .criteria()
            .any(|criterion| criterion.owner == "parent")
    {
        return Err(EngineError::bad(
            "Your parent did not pass criteria for this task. Do the work and report the result without creating criteria; ask your parent to pass criteria if this work needs them.",
        ));
    }
    let mut tail = None;
    match action {
        "set_objective" => {
            let objective = args["objective"]
                .as_str()
                .or_else(|| args["text"].as_str())
                .ok_or_else(|| EngineError::bad("objective required"))?;
            if objective != contract.objective && contract.criteria().any(|c| c.owner != "self") {
                return Err(EngineError::bad(
                    "the owner supplied this contract objective; use propose on the affected criterion or raise a roadblock",
                ));
            }
            contract.objective = objective.into();
        }
        "add" => {
            let c = Criterion {
                text: field(args, "text")?.into(),
                check: args
                    .get("check")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| EngineError::bad(e.to_string()))?,
                ..Criterion::default()
            };
            match field(args, "kind")? {
                "acceptance" => &mut contract.acceptance,
                "findings" => &mut contract.findings,
                "rejection" => &mut contract.rejection,
                "nonGoal" => &mut contract.non_goals,
                "assumedScope" => &mut contract.assumed_scope,
                "step" => &mut contract.steps,
                _ => return Err(EngineError::bad("invalid criterion kind")),
            }
            .push(c);
        }
        "finding" => {
            let finding = finding_arguments(args)?;
            let id = field(args, "id")?;
            if !contract.findings.iter().any(|c| c.id == id) {
                return Err(EngineError::bad("finding requires a findings criterion"));
            }
            let c = contract.criterion_mut(id)?;
            merge_finding(c, finding);
            coverage(c, args)?;
            if c.override_note.is_some() {
                return Err(EngineError::bad(
                    "the owner closed this criterion with an override",
                ));
            }
            c.status = "open".into();
            c.checked_clean = false;
            c.verification = reported();
            c.updated = sessions::now();
        }
        "met" | "hit" => {
            let id = field(args, "id")?;
            let evidence = field(args, "evidence")?;
            bounded(evidence, true)?;
            if action == "hit" && !contract.rejection.iter().any(|c| c.id == id) {
                return Err(EngineError::bad("hit requires a rejection criterion"));
            }
            if action == "met" && !contract.completion_criteria().any(|c| c.id == id) {
                return Err(EngineError::bad(
                    "met requires an acceptance or findings criterion; use step for a checklist step",
                ));
            }
            let c = contract.criterion_mut(id)?;
            if c.override_note.is_some() && matches!(c.status.as_str(), "met" | "withdrawn") {
                return Ok(json!({"contract":contract,"outputTail":"The owner already closed this criterion with an override; its recorded reason is preserved and the waived check is not re-run."}).to_string());
            }
            if let Some(records) = args.get("findings") {
                let records: Vec<Finding> = serde_json::from_value(records.clone())
                    .map_err(|e| EngineError::bad(e.to_string()))?;
                for finding in records {
                    finding.validate()?;
                    merge_finding(c, finding);
                }
            }
            coverage(c, args)?;
            if let Some(clean) = args["checkedClean"].as_bool() {
                c.checked_clean = clean;
            }
            c.evidence = evidence.into();
            c.updated = sessions::now();
            if action == "met" {
                if let Some(check) = &c.check {
                    let result = run_check(state, check, cancel, barrier.clone(), tx).await?;
                    let failed = result.is_error
                        || result.canonical_call.arguments["command"].as_str()
                            != Some(check.command.as_str());
                    c.verification = if failed { "check-failed" } else { "verified" }.into();
                    c.status = if failed { "open" } else { "met" }.into();
                    tail = Some(
                        result
                            .output
                            .chars()
                            .rev()
                            .take(4096)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect::<String>(),
                    );
                } else {
                    c.verification = reported();
                    c.status = "met".into();
                }
            } else {
                c.status = "hit".into();
                let reason = format!("Rejection criterion hit: {}. {evidence}", c.text)
                    .chars()
                    .take(4096)
                    .collect();
                contract.roadblocks.push(Roadblock {
                    id: id_string(),
                    criterion_id: Some(id.into()),
                    reason,
                    options: vec![],
                    resolution: None,
                    note: None,
                    created: sessions::now(),
                    updated: sessions::now(),
                });
            }
        }
        "step" => {
            let id = field(args, "id")?;
            if !contract.steps.iter().any(|c| c.id == id) {
                return Err(EngineError::bad("step requires a checklist step ID"));
            }
            let c = contract.criterion_mut(id)?;
            if c.owner != "self" {
                return Err(EngineError::bad(
                    "owned criteria cannot be edited; use criteria propose",
                ));
            }
            let status = field(args, "status")?;
            if !["open", "met", "withdrawn"].contains(&status) {
                return Err(EngineError::bad(
                    "step status must be open, met or withdrawn",
                ));
            }
            c.status = status.into();
            c.updated = sessions::now();
        }
        "propose" => {
            let id = field(args, "id")?;
            if contract.criterion_mut(id)?.owner == "self" {
                return Err(EngineError::bad(
                    "propose targets only user/parent-owned criteria",
                ));
            }
            contract.proposals.push(Proposal {
                id: id_string(),
                criterion_id: id.into(),
                change: field(args, "change")?.into(),
                check: serde_json::from_value(args["check"].clone())
                    .map_err(|e| EngineError::bad(e.to_string()))?,
                reason: field(args, "reason")?.into(),
                status: "open".into(),
                resolution: None,
                created: sessions::now(),
                updated: sessions::now(),
            });
        }
        "roadblock" => contract.roadblocks.push(Roadblock {
            id: id_string(),
            criterion_id: args["id"].as_str().map(str::to_owned),
            reason: field(args, "reason")?.into(),
            options: args["options"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            resolution: None,
            note: None,
            created: sessions::now(),
            updated: sessions::now(),
        }),
        _ => {
            return Err(EngineError::bad(
                "unsupported criteria action; owned criteria cannot be edited or withdrawn: use criteria propose",
            ));
        }
    }
    barrier
        .commit(|| {
            if cancel.is_cancelled() {
                return Err(EngineError::cancelled().into());
            }
            state.control.save_contract(target, contract.clone())?;
            if action == "propose" {
                let p = contract.proposals.last().unwrap();
                let owner = &contract.criteria().find(|c| c.id == p.criterion_id).unwrap().owner;
                if owner == "parent" {
                    let parent = state.control.journal.lock().unwrap().agents[target].parent_id.clone()
                        .ok_or_else(|| EngineError::bad("parent unavailable"))?;
                    state.control.message(target, &parent, format!(
                        "<criteria-proposal agentId=\"{target}\" proposalId=\"{}\">{} Use criteria.resolve_proposal(agentId=..., proposalId=..., resolution='accept'|'edit'|'reject', text=...).</criteria-proposal>",
                        p.id, serde_json::to_string(p).unwrap()
                    ))?;
                } else if owner == "user" {
                    state.control.emit(EngineEvent::ProposalRequest { agent_id: target.into(), proposal: p.clone() });
                } else {
                    return Err(EngineError::bad("self criteria need no owner approval").into());
                }
            }
            if target == "root"
                && let Some(roadblock) = contract.roadblock().cloned()
            {
                state.control.goal_roadblocked()?;
                state.control.emit(EngineEvent::RoadblockRequest { agent_id: target.into(), roadblock });
            }
            Ok(json!({"contract":state.control.contract(target),"outputTail":tail}).to_string())
        })
        .map_err(|e| EngineError::bad(e.to_string()))
}
fn id_string() -> String {
    id()
}
pub(crate) async fn run_check(
    state: &State,
    check: &CriterionCheck,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
    tx: &crate::model::EventSender,
) -> Result<crate::tools::ToolOutput, EngineError> {
    let cwd = crate::paths::Resolver::new(state.cwd.clone())
        .resolve(check.cwd.as_ref().and_then(|p| p.to_str()).unwrap_or("."))
        .map_err(|e| EngineError::bad(e.to_string()))?;
    let resolver = crate::paths::Resolver::new(state.cwd.clone());
    resolver
        .validate(&cwd)
        .map_err(|e| EngineError::bad(e.to_string()))?;
    if !cwd.starts_with(&state.cwd) {
        let call = ToolCall {
            id: id(),
            name: "read".into(),
            arguments: json!({"path":cwd}),
            complete: true,
        };
        state
            .gate
            .authorize(&state.metadata_operation(call, Some(cwd.clone())), cancel)
            .await
            .map_err(|e| EngineError::bad(e.to_string()))?;
    }
    let tools = state.tools.fork_for_agent(
        state.agent_id.clone(),
        state.control.breadcrumb(&state.agent_id),
        cwd,
    );
    let call = ToolCall {
        id: id(),
        name: "bash".into(),
        arguments: json!({"command":check.command,"timeout_secs":check.timeout_secs.unwrap_or(120)}),
        complete: true,
    };
    state.control.commit(
        &state.agent_id,
        HistoryEvent::ToolCall { call: call.clone() },
    )?;
    let output = tools.execute(call, cancel.clone(), barrier, tx).await;
    state.control.commit(
        &state.agent_id,
        HistoryEvent::ToolResult {
            details: Default::default(),
            id: output.canonical_call.id.clone(),
            name: "bash".into(),
            output: output.output.clone(),
            is_error: output.is_error,
        },
    )?;
    Ok(output)
}
fn query(state: &State, args: &Value) -> Result<String, EngineError> {
    let j = state.control.journal.lock().unwrap();
    let ids = crate::session_tools::scope(
        &j.agents,
        &state.agent_id,
        args["scope"].as_str().unwrap_or("self"),
        args["depth"].as_u64(),
    );
    let statuses = args["status"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_else(|| args["status"].as_str().into_iter().collect());
    let mut rows=ids.into_iter().filter_map(|id|{
        let a=j.agents.get(&id)?;if a.hidden(){return None;}let c=j.contracts.get(&id).cloned().unwrap_or_default();
        if !statuses.is_empty() && !statuses.contains(&a.status.as_str()) && !c.criteria().any(|c|statuses.contains(&c.status.as_str())) {return None;}
        if args["agent"].as_str().is_some_and(|s|a.agent!=s) || args["text"].as_str().is_some_and(|s|!serde_json::to_string(&c).unwrap().to_lowercase().contains(&s.to_lowercase())) {return None;}
        let breadcrumb=crate::session_tools::breadcrumb(&j.agents,&id);
        let depth=breadcrumb.matches('›').count();
        if args["depth"].as_u64().is_some_and(|n|depth as u64>n) && args["scope"].as_str()==Some("tree") {return None;}
        let updated=c.criteria().map(|c|c.updated.as_str()).chain(c.proposals.iter().map(|p|p.updated.as_str())).chain(c.roadblocks.iter().map(|r|r.updated.as_str())).chain(j.agent_updated.get(&id).map(String::as_str)).max();
        Some(json!({"agentId":id,"breadcrumb":breadcrumb,"agentState":a.status,"counts":c.counts(),"roadblock":c.roadblock(),"contract":c,"depth":depth,"created":a.created_at,"updated":updated}))
    }).collect::<Vec<_>>();
    match args["sort"].as_str().unwrap_or("updated") {
        "depth" => rows.sort_by_key(|r| r["depth"].as_u64()),
        "created" => rows.sort_by(|a, b| b["created"].as_str().cmp(&a["created"].as_str())),
        _ => rows.sort_by(|a, b| b["updated"].as_str().cmp(&a["updated"].as_str())),
    }
    rows.truncate(args["limit"].as_u64().unwrap_or(100).min(1000) as usize);
    Ok(serde_json::to_string(&rows).unwrap())
}

pub(crate) fn task_criteria_schema() -> Value {
    json!({"type":"array","maxItems":32,"items":{"anyOf":[{"type":"string","maxLength":4096},{"type":"object","properties":{"id":{"type":"string"},"text":{"type":"string","maxLength":4096},"check":{"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeoutSecs":{"type":"integer","minimum":1,"maximum":600}},"required":["command"],"additionalProperties":false}},"required":["text"],"additionalProperties":false}]}})
}
