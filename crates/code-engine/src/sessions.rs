//! Append-only session journal and token ownership. No database or prune ledger.
use crate::schema::*;
use code_config::{ConfigStore, PermissionMode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};
use uuid::Uuid;
type Result<T> = std::result::Result<T, EngineError>;
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
fn bad(e: impl std::fmt::Display) -> EngineError {
    EngineError::bad(format!("session storage: {e}"))
}
fn valid_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(EngineError::bad("invalid session/agent id"));
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkPoint {
    pub session_id: String,
    pub agent_id: String,
    pub context: u32,
    pub line: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub id: String,
    pub cwd: PathBuf,
    pub created_at: String,
    pub title: String,
    #[serde(default)]
    pub title_source: TitleSource,
    #[serde(default)]
    pub title_turns: u64,
    #[serde(default)]
    pub title_generation: u64,
    pub fork_of: Option<ForkPoint>,
    #[serde(default)]
    pub goal: Option<crate::goal_controller::GoalRun>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionSettings {
    pub subagents: code_config::SubagentMode,
    pub exclusive: Option<crate::Exclusive>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<PermissionMode>,
    /// Shared safety reviewer for the complete tree, defaulted from permissions.autoModel.
    pub safety_model: Option<String>,
    pub mcps: Option<BTreeSet<String>>,
    pub disabled_models: BTreeSet<String>,
    pub disabled_providers: BTreeSet<String>,
    pub compact_at: BTreeMap<String, f64>,
    pub strict_tools: BTreeMap<String, bool>,
    /// D138 server resolution, including empty results, frozen for the session.
    pub lsp_servers: Option<BTreeMap<String, code_config::lsp::LspServer>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub agent: String,
    pub model: Option<String>,
    pub status: String,
    pub created_at: String,
    pub stable_prompt: String,
}
impl AgentRecord {
    pub(crate) fn hidden(&self) -> bool {
        matches!(self.agent.as_str(), "auto-title" | "goal review")
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockRecord {
    pub pid: u32,
    pub host: String,
    pub started_at: String,
    pub token: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockStatus {
    #[serde(default)]
    pub title: String,
    pub holder: Option<LockRecord>,
    pub owned: bool,
    pub read_only: bool,
    pub pid_alive: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub title: String,
    pub model: Option<String>,
    pub cost: String,
    pub totals: SessionTotals,
    pub header: SessionHeader,
    pub lock: LockStatus,
    pub updated_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub version: u32,
    pub header: SessionHeader,
    pub settings: SessionSettings,
    pub agents: BTreeMap<String, AgentRecord>,
    pub contexts: BTreeMap<String, BTreeMap<u32, Vec<EventEnvelope>>>,
}
/// Stable advisory inode closes token-check/append/claim races across processes.
struct Guard(File);
impl Guard {
    fn acquire(dir: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(".guard"))
            .map_err(bad)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(bad(std::io::Error::last_os_error()));
        }
        Ok(Self(file))
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
fn host() -> String {
    let mut bytes = [0u8; 256];
    if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } == 0 {
        String::from_utf8_lossy(&bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())])
            .into_owned()
    } else {
        "unknown-host".into()
    }
}
fn status(dir: &Path, token: Option<&str>, read_only: bool) -> Result<LockStatus> {
    let holder: Option<LockRecord> = match fs::read(dir.join("lock.json")) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(bad)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(bad(e)),
    };
    let alive = holder.as_ref().filter(|h| h.host == host()).map(|h| {
        let rc = unsafe { libc::kill(h.pid as i32, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    });
    Ok(LockStatus {
        title: read_json::<SessionHeader>(&dir.join("session.json"))?.title,
        owned: holder
            .as_ref()
            .is_some_and(|h| Some(h.token.as_str()) == token),
        holder,
        read_only,
        pid_alive: alive,
    })
}
pub fn lock_status(data: &Path, id: &str) -> Result<LockStatus> {
    valid_id(id)?;
    status(&data.join("sessions").join(id), None, false)
}
pub fn list(cwd: &Path) -> Result<Vec<SessionInfo>> {
    list_in(
        &code_config::Directories::resolve().map_err(bad)?.data,
        Some(cwd),
    )
}
/// A damaged session is visible here without preventing healthy picker rows.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListProblem {
    pub path: PathBuf,
    pub error: EngineError,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionListing {
    pub sessions: Vec<SessionInfo>,
    pub problems: Vec<SessionListProblem>,
}
pub fn list_in(data: &Path, cwd: Option<&Path>) -> Result<Vec<SessionInfo>> {
    Ok(list_with_problems_in(data, cwd)?.sessions)
}
pub fn list_with_problems_in(data: &Path, cwd: Option<&Path>) -> Result<SessionListing> {
    let root = data.join("sessions");
    let mut listing = SessionListing::default();
    let entries = match fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(listing),
        Err(e) => return Err(bad(e)),
    };
    let cwd = cwd.map(fs::canonicalize).transpose().map_err(bad)?;
    for entry in entries {
        let mut path = root.clone();
        let result = (|| {
            let entry = entry.map_err(bad)?;
            path = entry.path();
            if !entry.file_type().map_err(bad)?.is_dir() {
                return Ok(None);
            }
            let header: SessionHeader = read_json(&path.join("session.json"))?;
            if cwd.as_ref().is_some_and(|cwd| *cwd != header.cwd) {
                return Ok(None);
            }
            let settings: SessionSettings = read_json(&path.join("settings.json"))?;
            let root_agent: AgentRecord = read_json(&path.join("agents/root/agent.json"))?;
            let mut updated = header.created_at.clone();
            let mut totals = SessionTotals::default();
            let mut calls = BTreeSet::new();
            for files in read_contexts(&path)?.values() {
                for events in files.values() {
                    for envelope in events {
                        updated = updated.max(envelope.ts.clone());
                        if let HistoryEvent::Usage { usage } = &envelope.event
                            && calls.insert(usage.call_id.clone())
                        {
                            totals.add(usage);
                        }
                    }
                }
            }
            Ok(Some(SessionInfo {
                title: header.title.clone(),
                header,
                model: settings.model.or(root_agent.model),
                cost: totals.cost_display(),
                totals,
                lock: status(&path, None, false)?,
                updated_at: updated,
            }))
        })();
        match result {
            Ok(Some(row)) => listing.sessions.push(row),
            Ok(None) => {}
            Err(error) => listing.problems.push(SessionListProblem { path, error }),
        }
    }
    listing
        .sessions
        .sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(listing)
}
/// Rename a session that is not open here (the TUI `/resume` picker): a
/// user title, recorded like `/rename`. Fails while another process holds it.
pub fn rename_session(store: &ConfigStore, id: &str, title: &str) -> Result<()> {
    // A stale lock (its process is gone) is not a hold.
    let stale = lock_status(&store.directories.data, id)
        .is_ok_and(|l| l.holder.is_some() && l.pid_alive == Some(false));
    let mut journal = Journal::load(store, id, stale)?;
    let result = journal
        .set_title(title.trim().to_string(), TitleSource::User, None)
        .map(|_| ());
    let closed = journal.close();
    result.and(closed)
}
/// Delete a session directory. Refused while the session is locked by a
/// process that may be alive (a stale lock of a dead process is not a hold).
pub fn delete_session(data: &Path, id: &str) -> Result<()> {
    valid_id(id)?;
    let dir = data.join("sessions").join(id);
    {
        let _guard = Guard::acquire(&dir)?;
        let lock = status(&dir, None, false)?;
        if lock.holder.is_some() && lock.pid_alive != Some(false) {
            return Err(EngineError {
                kind: ErrorKind::LockConflict,
                message: format!("session {id} is open in another process; not deleted"),
                retryable: false,
                retry_after_secs: None,
            });
        }
    }
    fs::remove_dir_all(&dir).map_err(bad)
}
/// The last `limit` user and assistant messages of a session's root agent
/// (newest context only), oldest first, for a picker preview.
pub fn recent_messages(data: &Path, id: &str, limit: usize) -> Result<Vec<(String, String)>> {
    valid_id(id)?;
    let dir = data.join("sessions").join(id).join("agents/root/contexts");
    let newest = fs::read_dir(&dir)
        .map_err(bad)?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let n = p.file_stem()?.to_str()?.parse::<u32>().ok()?;
            (p.extension()? == "jsonl").then_some((n, p))
        })
        .max_by_key(|(n, _)| *n);
    let Some((_, path)) = newest else {
        return Ok(vec![]);
    };
    let mut out: Vec<(String, String)> = vec![];
    for line in BufReader::new(File::open(&path).map_err(bad)?).lines() {
        let Ok(envelope) = serde_json::from_str::<EventEnvelope>(&line.map_err(bad)?) else {
            continue;
        };
        match envelope.event {
            HistoryEvent::User { text, .. }
                if !text.trim_start().starts_with("<system-message") =>
            {
                out.push(("user".into(), text))
            }
            HistoryEvent::Assistant { text, .. } if !text.trim().is_empty() => {
                out.push(("assistant".into(), text))
            }
            _ => {}
        }
    }
    let skip = out.len().saturating_sub(limit);
    Ok(out.split_off(skip))
}
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).map_err(bad)?).map_err(bad)
}
pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().ok_or_else(|| bad("no parent"))?)
        .map_err(bad)?;
    serde_json::to_writer_pretty(&mut temp, value).map_err(bad)?;
    temp.write_all(b"\n").map_err(bad)?;
    temp.as_file().sync_all().map_err(bad)?;
    temp.persist(path).map_err(bad)?;
    Ok(())
}
fn context_path(dir: &Path, agent: &str, context: u32) -> PathBuf {
    dir.join("agents")
        .join(agent)
        .join("contexts")
        .join(format!("{context:04}.jsonl"))
}
pub(crate) fn read_contexts(
    dir: &Path,
) -> Result<BTreeMap<String, BTreeMap<u32, Vec<EventEnvelope>>>> {
    let mut all = BTreeMap::new();
    for agent in fs::read_dir(dir.join("agents")).map_err(bad)? {
        let agent = agent.map_err(bad)?;
        if !agent.file_type().map_err(bad)?.is_dir() {
            continue;
        }
        let id = agent
            .file_name()
            .to_str()
            .ok_or_else(|| bad("invalid agent id"))?
            .to_owned();
        valid_id(&id)?;
        let mut contexts = BTreeMap::new();
        for entry in fs::read_dir(agent.path().join("contexts")).map_err(bad)? {
            let path = entry.map_err(bad)?.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let n = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or_else(|| bad("invalid context filename"))?;
            let mut events = vec![];
            for line in BufReader::new(File::open(&path).map_err(bad)?).lines() {
                let line = line.map_err(bad)?;
                let mut event: EventEnvelope = serde_json::from_str(&line).map_err(|_| {
                    bad(format!(
                        "invalid event at {}:{}; refusing to alter canonical history",
                        path.display(),
                        events.len() + 1
                    ))
                })?;
                if event.version != 1 {
                    return Err(bad("unsupported session event version"));
                }
                crate::injection::canonicalize(&mut event.event, &event.id);
                events.push(event);
            }
            contexts.insert(n, events);
        }
        all.insert(id, contexts);
    }
    Ok(all)
}
pub(crate) struct Journal {
    pub guidance_selections: BTreeMap<String, crate::guidance_selector::GuidanceSelection>,
    pub subscriptions: BTreeMap<String, Vec<PathBuf>>,
    pub subscription_pending: BTreeMap<String, crate::subscriptions::Pending>,
    /// D122 local command notices not yet delivered, per agent.
    pub local_commands: BTreeMap<String, VecDeque<String>>,
    pub wait_edges: BTreeMap<String, (String, String)>,
    pub scopes: BTreeMap<String, crate::scopes::AgentScope>,
    pub scope_requests: BTreeMap<String, crate::scopes::ScopeRequest>,
    /// Latest revision journaled for each request's owner.
    pub scope_owner_records: BTreeMap<String, String>,
    /// User resolutions already queued once; rebuilt from durable owner events.
    pub scope_resolution_notices: BTreeSet<String>,
    pub model_turns: BTreeMap<String, u64>,
    pub model_reminders: BTreeMap<(String, String), u64>,
    pub dir: PathBuf,
    pub header: SessionHeader,
    pub settings: SessionSettings,
    pub agents: BTreeMap<String, AgentRecord>,
    pub histories: BTreeMap<String, Vec<HistoryEvent>>,
    pub contexts: BTreeMap<String, u32>,
    pub totals: SessionTotals,
    pub agent_totals: BTreeMap<String, SessionTotals>,
    pub contracts: BTreeMap<String, crate::goal::GoalContract>,
    pub questions: BTreeMap<String, QuestionRecord>,
    pub mailbox: BTreeMap<String, VecDeque<String>>,
    pub root_mail_suspended: bool,
    token: String,
    pub read_only: bool,
    closed: bool,
    persisted: bool,
    pub release_on_drop: bool,
    pub requests: BTreeMap<String, crate::model::ChildRequest>,
    pub agent_updated: BTreeMap<String, String>,
    pub agent_activity: BTreeMap<String, String>,
    pub last_actions: BTreeMap<String, String>,
    pub last_tools: BTreeMap<String, String>,
    pub agent_reports: BTreeMap<String, String>,
    pub partial_reports: BTreeMap<String, String>,
    pub finalizing: BTreeSet<String>,
    pub growth_pending: BTreeMap<String, Vec<crate::reporting::GrowthNotice>>,
    pub decisions: BTreeMap<String, crate::DecisionRecord>,
    sequence: u64,
    pub turn_id: Option<String>,
    pub attempt_id: Option<String>,
    pub activities: BTreeMap<String, (String, Option<String>)>,
    pub jobs: BTreeMap<String, JobRecord>,
    pub executions: BTreeMap<String, AgentExecution>,
    pub descendant_launches: BTreeMap<String, u64>,
    launches_seen: BTreeSet<String>,
    message_sequences: BTreeMap<(String, String), u64>,
}
impl Journal {
    pub(crate) fn is_persisted(&self) -> bool {
        self.persisted
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    pub fn sequence_number(&self) -> u64 {
        self.sequence
    }
    pub fn latest_message_sequence(&self, to: &str, from: &str) -> u64 {
        self.message_sequences
            .get(&(to.into(), from.into()))
            .copied()
            .unwrap_or(0)
    }

    pub fn create(store: &ConfigStore, cwd: PathBuf, settings: SessionSettings) -> Result<Self> {
        let root = store.directories.data.join("sessions");
        let id = Uuid::new_v4().to_string();
        let dir = root.join(&id);
        let header = SessionHeader {
            id,
            cwd,
            created_at: now(),
            title: String::new(),
            title_source: TitleSource::Auto,
            title_turns: 0,
            title_generation: 0,
            fork_of: None,
            goal: None,
        };
        let agent = AgentRecord {
            agent_id: "root".into(),
            parent_id: None,
            agent: "build".into(),
            model: settings
                .model
                .clone()
                .or(store.snapshot().config.default_model.clone()),
            status: "waiting".into(),
            created_at: now(),
            stable_prompt: crate::BUILD_PROMPT.into(),
        };
        Ok(Self {
            dir,
            header,
            settings,
            agents: BTreeMap::from([("root".into(), agent)]),
            histories: BTreeMap::new(),
            contexts: BTreeMap::from([("root".into(), 1)]),
            totals: SessionTotals::default(),
            agent_totals: BTreeMap::new(),
            contracts: BTreeMap::new(),
            questions: BTreeMap::new(),
            mailbox: BTreeMap::new(),
            root_mail_suspended: false,
            token: Uuid::new_v4().to_string(),
            read_only: false,
            closed: false,
            persisted: false,
            release_on_drop: true,
            requests: BTreeMap::new(),
            agent_updated: BTreeMap::new(),
            agent_activity: BTreeMap::new(),
            last_actions: BTreeMap::new(),
            last_tools: BTreeMap::new(),
            agent_reports: BTreeMap::new(),
            partial_reports: BTreeMap::new(),
            finalizing: BTreeSet::new(),
            growth_pending: BTreeMap::new(),
            decisions: BTreeMap::new(),
            sequence: 0,
            turn_id: None,
            attempt_id: None,
            activities: BTreeMap::new(),
            jobs: BTreeMap::new(),
            executions: BTreeMap::new(),
            descendant_launches: BTreeMap::new(),
            launches_seen: BTreeSet::new(),
            message_sequences: BTreeMap::new(),
            guidance_selections: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            subscription_pending: BTreeMap::new(),
            local_commands: BTreeMap::new(),
            wait_edges: BTreeMap::new(),
            scopes: BTreeMap::new(),
            scope_requests: BTreeMap::new(),
            scope_owner_records: BTreeMap::new(),
            scope_resolution_notices: BTreeSet::new(),
            model_turns: BTreeMap::new(),
            model_reminders: BTreeMap::new(),
        })
    }
    /// Materialize and lock only when an operation needs durable state.
    pub fn ensure_persisted(&mut self) -> Result<()> {
        if self.closed {
            return Err(bad("session is closed"));
        }
        if self.persisted {
            return Ok(());
        }
        fs::create_dir_all(self.dir.parent().unwrap()).map_err(bad)?;
        fs::create_dir(&self.dir).map_err(bad)?;
        let result = (|| {
            let _guard = Guard::acquire(&self.dir)?;
            let lock = LockRecord {
                pid: std::process::id(),
                host: host(),
                started_at: now(),
                token: self.token.clone(),
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.dir.join("lock.json"))
                .map_err(bad)?;
            serde_json::to_writer(&mut file, &lock).map_err(bad)?;
            file.sync_all().map_err(bad)?;
            fs::create_dir(self.dir.join("assets")).map_err(bad)?;
            for agent in self.agents.values() {
                let path = self.dir.join("agents").join(&agent.agent_id);
                fs::create_dir_all(path.join("contexts")).map_err(bad)?;
                write_json(&path.join("agent.json"), agent)?;
                File::create(context_path(
                    &self.dir,
                    &agent.agent_id,
                    self.contexts[&agent.agent_id],
                ))
                .map_err(bad)?;
            }
            write_json(&self.dir.join("settings.json"), &self.settings)?;
            write_json(&self.dir.join("session.json"), &self.header)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&self.dir);
        }
        result?;
        self.persisted = true;
        Ok(())
    }
    pub fn load(store: &ConfigStore, id: &str, claim: bool) -> Result<Self> {
        valid_id(id)?;
        let dir = store.directories.data.join("sessions").join(id);
        let _guard = Guard::acquire(&dir)?;
        let header: SessionHeader = read_json(&dir.join("session.json"))?;
        let existing = status(&dir, None, false)?;
        if let Some(h) = existing.holder
            && !claim
        {
            return Err(EngineError {
                kind: ErrorKind::LockConflict,
                message: format!(
                    "session {id} locked by pid {} on {} since {} (pid alive here: {:?}); use --fork {id} or --resume {id} --claim-lock",
                    h.pid, h.host, h.started_at, existing.pid_alive
                ),
                retryable: false,
                retry_after_secs: None,
            });
        }
        let lock = LockRecord {
            pid: std::process::id(),
            host: host(),
            started_at: now(),
            token: Uuid::new_v4().to_string(),
        };
        if claim {
            // The stable .guard serializes claim with every writer's check+append.
            write_json(&dir.join("lock.json"), &lock)?;
        } else {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dir.join("lock.json"))
                .map_err(bad)?;
            serde_json::to_writer(&mut file, &lock).map_err(bad)?;
            file.sync_all().map_err(bad)?;
        }
        let mut restarted_goal = None;
        let result = (|| {
            let settings = read_json(&dir.join("settings.json"))?;
            let all = read_contexts(&dir)?;
            let mut agents = BTreeMap::new();
            for id in all.keys() {
                let record: AgentRecord =
                    read_json(&dir.join("agents").join(id).join("agent.json"))?;
                agents.insert(id.clone(), record);
            }
            let mut journal = Self {
                dir: dir.clone(),
                header,
                settings,
                agents,
                histories: BTreeMap::new(),
                contexts: BTreeMap::new(),
                totals: SessionTotals::default(),
                agent_totals: BTreeMap::new(),
                contracts: BTreeMap::new(),
                questions: BTreeMap::new(),
                mailbox: BTreeMap::new(),
                root_mail_suspended: false,
                token: lock.token.clone(),
                read_only: false,
                closed: false,
                persisted: true,
                release_on_drop: true,
                requests: BTreeMap::new(),
                agent_updated: BTreeMap::new(),
                agent_activity: BTreeMap::new(),
                last_actions: BTreeMap::new(),
                last_tools: BTreeMap::new(),
                agent_reports: BTreeMap::new(),
                partial_reports: BTreeMap::new(),
                finalizing: BTreeSet::new(),
                growth_pending: BTreeMap::new(),
                decisions: BTreeMap::new(),
                sequence: 0,
                turn_id: None,
                attempt_id: None,
                activities: BTreeMap::new(),
                jobs: BTreeMap::new(),
                executions: BTreeMap::new(),
                descendant_launches: BTreeMap::new(),
                launches_seen: BTreeSet::new(),
                message_sequences: BTreeMap::new(),
                guidance_selections: BTreeMap::new(),
                subscriptions: BTreeMap::new(),
                subscription_pending: BTreeMap::new(),
                local_commands: BTreeMap::new(),
                wait_edges: BTreeMap::new(),
                scopes: BTreeMap::new(),
                scope_requests: BTreeMap::new(),
                scope_owner_records: BTreeMap::new(),
                scope_resolution_notices: BTreeSet::new(),
                model_turns: BTreeMap::new(),
                model_reminders: BTreeMap::new(),
            };
            let mut metadata_events = vec![];
            for (id, contexts) in all {
                let newest = contexts.keys().next_back().copied().unwrap_or(1);
                journal.contexts.insert(id.clone(), newest);
                for (n, events) in contexts {
                    for envelope in events {
                        journal.sequence = journal.sequence.max(envelope.sequence);
                        metadata_events.push((id.clone(), envelope.clone()));
                        if n == newest {
                            reduce(
                                journal.histories.entry(id.clone()).or_default(),
                                &envelope.event,
                            );
                        }
                    }
                }
            }
            metadata_events.sort_by(|(_, a), (_, b)| {
                a.sequence.cmp(&b.sequence).then_with(|| a.ts.cmp(&b.ts))
            });
            for (id, envelope) in metadata_events {
                // A fork owns new title state; copied ancestor events retain their session ID.
                if !matches!(envelope.event, HistoryEvent::TitleChange { .. })
                    || envelope.session_id == journal.header.id
                {
                    if let Some(record) = crate::decisions::automatic(&envelope) {
                        journal.decisions.entry(record.id.clone()).or_insert(record);
                    }
                    journal.metadata(&id, &envelope.event);
                    journal
                        .agent_activity
                        .insert(id.clone(), envelope.ts.clone());
                }
                if matches!(envelope.event, HistoryEvent::AgentState { .. }) {
                    journal.agent_updated.insert(id.clone(), envelope.ts);
                }
            }
            if let Some(run) = &mut journal.header.goal {
                if run.disposition == "running" {
                    run.transition("paused", Some("restart".into()));
                    restarted_goal = Some(run.clone());
                }
                write_json(&journal.dir.join("session.json"), &journal.header)?;
            }
            // Metadata replay already consumed delivered reminders in sequence order.
            Ok(journal)
        })();
        if result.is_err() {
            let _ = fs::remove_file(dir.join("lock.json"));
        }
        drop(_guard);
        let mut journal = result?;
        if let Some(state) = restarted_goal {
            journal.append(
                "root",
                HistoryEvent::GoalState {
                    state: Box::new(state),
                },
            )?;
        }
        Ok(journal)
    }
    pub fn fork(store: &ConfigStore, id: &str, point: Option<ForkPoint>) -> Result<Self> {
        valid_id(id)?;
        let dir = store.directories.data.join("sessions").join(id);
        let _guard = Guard::acquire(&dir)?;
        let header = read_json(&dir.join("session.json"))?;
        let settings = read_json(&dir.join("settings.json"))?;
        let contexts = read_contexts(&dir)?;
        let mut agents = BTreeMap::new();
        for id in contexts.keys() {
            agents.insert(
                id.clone(),
                read_json(&dir.join("agents").join(id).join("agent.json"))?,
            );
        }
        let snapshot = SessionSnapshot {
            version: 1,
            header,
            settings,
            agents,
            contexts,
        };
        Self::restore(store, snapshot, point)
    }
    pub fn restore(
        store: &ConfigStore,
        mut snapshot: SessionSnapshot,
        point: Option<ForkPoint>,
    ) -> Result<Self> {
        if snapshot.version != 1 {
            return Err(bad("unsupported snapshot version"));
        }
        let source = snapshot.header.id.clone();
        let explicit_point = point.is_some();
        let point = point.unwrap_or_else(|| {
            let (context, events) = snapshot
                .contexts
                .get("root")
                .and_then(|c| c.last_key_value())
                .map(|(n, e)| (*n, e.len()))
                .unwrap_or((1, 0));
            ForkPoint {
                session_id: source.clone(),
                agent_id: "root".into(),
                context,
                line: events,
            }
        });
        valid_id(&point.agent_id)?;
        let events = snapshot
            .contexts
            .get(&point.agent_id)
            .and_then(|c| c.get(&point.context))
            .ok_or_else(|| bad("fork context not found"))?;
        if point.line > events.len() {
            return Err(bad("fork line is past end of context"));
        }
        let boundary = if !explicit_point {
            now()
        } else if point.line == 0 {
            snapshot
                .contexts
                .get(&point.agent_id)
                .and_then(|contexts| contexts.range(..point.context).next_back())
                .and_then(|(_, events)| events.last())
                .map(|e| e.ts.clone())
                .or_else(|| {
                    snapshot
                        .agents
                        .get(&point.agent_id)
                        .map(|a| a.created_at.clone())
                })
                .unwrap_or_else(|| snapshot.header.created_at.clone())
        } else {
            events[point.line - 1].ts.clone()
        };
        let id = Uuid::new_v4().to_string();
        let root = store.directories.data.join("sessions");
        fs::create_dir_all(&root).map_err(bad)?;
        let dir = root.join(&id);
        fs::create_dir(&dir).map_err(bad)?;
        snapshot.header.id = id.clone();
        snapshot.header.created_at = now();
        snapshot.header.title.clear();
        snapshot.header.title_source = TitleSource::Auto;
        snapshot.header.title_turns = 0;
        snapshot.header.title_generation = 0;
        snapshot.header.goal = None;
        snapshot.header.fork_of = Some(ForkPoint {
            session_id: source,
            ..point.clone()
        });
        write_json(&dir.join("session.json"), &snapshot.header)?;
        write_json(&dir.join("settings.json"), &snapshot.settings)?;
        fs::create_dir(dir.join("assets")).map_err(bad)?;
        let mut retained: BTreeSet<String> = snapshot
            .agents
            .iter()
            .filter(|(id, a)| id.as_str() == "root" || a.created_at <= boundary)
            .map(|(id, _)| id.clone())
            .collect();
        loop {
            let before = retained.len();
            retained.retain(|id| {
                snapshot.agents[id]
                    .parent_id
                    .as_ref()
                    .is_none_or(|p| retained_parent(&snapshot.agents, p, &boundary))
            });
            if before == retained.len() {
                break;
            }
        }
        for agent in retained {
            valid_id(&agent)?;
            let path = dir.join("agents").join(&agent);
            fs::create_dir_all(path.join("contexts")).map_err(bad)?;
            write_json(&path.join("agent.json"), &snapshot.agents[&agent])?;
            if let Some(contexts) = snapshot.contexts.get(&agent) {
                for (n, events) in contexts {
                    if agent == point.agent_id && *n > point.context {
                        continue;
                    }
                    let mut file = File::create(context_path(&dir, &agent, *n)).map_err(bad)?;
                    for (i, envelope) in events.iter().enumerate() {
                        if agent == point.agent_id && *n == point.context && i >= point.line {
                            break;
                        }
                        // A tree fork is one point in time, including siblings already alive then.
                        if envelope.ts > boundary {
                            break;
                        }
                        serde_json::to_writer(&mut file, envelope).map_err(bad)?;
                        file.write_all(b"\n").map_err(bad)?;
                    }
                    file.sync_all().map_err(bad)?;
                }
            }
        }
        // Assets are immutable attachments; copy without following symlink directories.
        let source_assets = store
            .directories
            .data
            .join("sessions")
            .join(&snapshot.header.fork_of.as_ref().unwrap().session_id)
            .join("assets");
        if source_assets.is_dir() {
            copy_assets(&source_assets, &dir.join("assets"))?;
        }
        let source_jobs = store
            .directories
            .data
            .join("sessions")
            .join(&snapshot.header.fork_of.as_ref().unwrap().session_id)
            .join("jobs");
        if source_jobs.is_dir() {
            copy_assets(&source_jobs, &dir.join("jobs"))?;
        }
        Self::load(store, &id, false)
    }
    pub fn snapshot(&self) -> Result<SessionSnapshot> {
        let _guard = if self.persisted {
            Some(Guard::acquire(&self.dir)?)
        } else {
            None
        };
        Ok(SessionSnapshot {
            version: 1,
            header: self.header.clone(),
            settings: self.settings.clone(),
            agents: self.agents.clone(),
            contexts: if self.persisted {
                read_contexts(&self.dir)?
            } else {
                BTreeMap::from([("root".into(), BTreeMap::from([(1, vec![])]))])
            },
        })
    }
    pub fn lock_status(&self) -> Result<LockStatus> {
        if !self.persisted {
            return Ok(LockStatus {
                title: self.header.title.clone(),
                holder: None,
                owned: false,
                read_only: self.read_only,
                pid_alive: None,
            });
        }
        let _guard = Guard::acquire(&self.dir)?;
        status(&self.dir, Some(&self.token), self.read_only)
    }
    fn verify(&mut self) -> Result<()> {
        if self.closed {
            return Err(EngineError::bad("session is closed"));
        }
        if self.read_only || !status(&self.dir, Some(&self.token), self.read_only)?.owned {
            self.read_only = true;
            return Err(EngineError {
                kind: ErrorKind::ReadOnly,
                message: "session claimed by another process".into(),
                retryable: false,
                retry_after_secs: None,
            });
        }
        Ok(())
    }
    pub fn with_owner<T>(
        &mut self,
        action: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.ensure_persisted()?;
        let _guard = Guard::acquire(&self.dir)?;
        self.verify()?;
        action()
    }
    pub fn check_owner(&mut self) -> Result<()> {
        if !self.persisted {
            return if self.closed {
                Err(bad("session is closed"))
            } else {
                Ok(())
            };
        }
        let _guard = Guard::acquire(&self.dir)?;
        self.verify()
    }
    pub fn append(&mut self, agent: &str, mut event: HistoryEvent) -> Result<EventEnvelope> {
        self.ensure_persisted()?;
        let _guard = Guard::acquire(&self.dir)?;
        self.verify()?;
        self.sequence += 1;
        let attempt_id = match &event {
            HistoryEvent::Usage { usage } => Some(usage.call_id.clone()),
            HistoryEvent::User { .. }
            | HistoryEvent::SystemReminder { .. }
            | HistoryEvent::SettingChange { .. }
            | HistoryEvent::Message { .. }
            | HistoryEvent::Answer { .. }
            | HistoryEvent::QuestionsAnswered { .. } => None,
            _ => self.activities.get(agent).and_then(|a| a.1.clone()),
        };
        let event_id = Uuid::new_v4().to_string();
        crate::injection::canonicalize_fresh(&mut event, &event_id);
        let envelope = EventEnvelope {
            version: 1,
            id: event_id,
            session_id: self.header.id.clone(),
            agent_id: agent.into(),
            turn_id: self.activities.get(agent).map(|a| a.0.clone()),
            attempt_id,
            sequence: self.sequence,
            ts: now(),
            event,
        };
        let context = *self
            .contexts
            .get(agent)
            .ok_or_else(|| bad("agent is not registered"))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(context_path(&self.dir, agent, context))
            .map_err(bad)?;
        let mut bytes = serde_json::to_vec(&envelope).map_err(bad)?;
        bytes.push(b'\n');
        file.write_all(&bytes).map_err(bad)?;
        file.sync_all().map_err(bad)?;
        self.metadata(agent, &envelope.event);
        self.agent_activity
            .insert(agent.into(), envelope.ts.clone());
        if matches!(envelope.event, HistoryEvent::GoalState { .. })
            || (agent == "root" && matches!(envelope.event, HistoryEvent::Goal { .. }))
        {
            write_json(&self.dir.join("session.json"), &self.header)?;
        }
        if matches!(envelope.event, HistoryEvent::AgentState { .. }) {
            self.agent_updated.insert(agent.into(), envelope.ts.clone());
        }
        reduce(
            self.histories.entry(agent.into()).or_default(),
            &envelope.event,
        );
        Ok(envelope)
    }
    fn deliver_answer(&mut self, id: &str, text: String, answers: Option<Vec<QuestionAnswer>>) {
        if let Some(q) = self.questions.get_mut(id) {
            q.answer = Some(text.clone());
            q.answers = answers;
            let finished = self
                .agents
                .get(&q.asker)
                .is_some_and(|a| matches!(a.status.as_str(), "done" | "failed" | "killed"));
            let recipient = if finished {
                self.agents
                    .get(&q.asker)
                    .and_then(|a| a.parent_id.clone())
                    .unwrap_or_else(|| q.asker.clone())
            } else {
                q.asker.clone()
            };
            if !q.blocking || finished {
                self.mailbox
                    .entry(recipient)
                    .or_default()
                    .push_back(format!("Answer to question {id}: {text}"));
            }
        }
    }
    fn metadata(&mut self, agent: &str, event: &HistoryEvent) {
        match event {
            HistoryEvent::DecisionRecorded { record }
            | HistoryEvent::DecisionReviewed { record } => {
                self.decisions.insert(record.id.clone(), record.clone());
            }
            HistoryEvent::ToolCall { call } => {
                self.last_actions
                    .insert(agent.into(), format!("{} started", call.name));
                self.last_tools.insert(agent.into(), call.name.clone());
            }
            HistoryEvent::ScriptCall { call, result, .. } => {
                let state = result.as_ref().map_or("started", |result| {
                    if result.is_error {
                        "failed"
                    } else {
                        "completed"
                    }
                });
                self.last_actions
                    .insert(agent.into(), format!("{} {state}", call.name));
                self.last_tools.insert(agent.into(), call.name.clone());
            }
            HistoryEvent::ToolResult { name, is_error, .. } => {
                self.last_actions.insert(
                    agent.into(),
                    format!("{name} {}", if *is_error { "failed" } else { "completed" }),
                );
                self.last_tools.insert(agent.into(), name.clone());
            }
            HistoryEvent::RootMailSuspended => {
                self.root_mail_suspended = true;
            }
            HistoryEvent::User { .. } if agent == "root" => {
                self.root_mail_suspended = false;
            }
            HistoryEvent::AgentFinalizeRequested { agents, .. } => {
                self.finalizing.extend(agents.iter().cloned());
            }
            HistoryEvent::AgentFinalized => {
                self.finalizing.remove(agent);
            }
            HistoryEvent::AgentPartialReport { agent_id, text } => {
                self.partial_reports.insert(agent_id.clone(), text.clone());
                if agent == agent_id
                    && let Some(parent) =
                        self.agents.get(agent_id).and_then(|a| a.parent_id.clone())
                {
                    let prefix = crate::reporting::partial_prefix(agent_id);
                    let mail = self.mailbox.entry(parent).or_default();
                    mail.retain(|message| !message.starts_with(&prefix));
                    mail.push_back(format!("{prefix}{text}"));
                }
            }
            HistoryEvent::SubtreeGrowthDelivered { launches } => {
                if let Some(pending) = self.growth_pending.get_mut(agent) {
                    pending.retain(|notice| !launches.contains(&notice.launch_id));
                }
            }
            HistoryEvent::AgentReport { report, .. } => {
                self.agent_reports.insert(agent.into(), report.clone());
            }
            HistoryEvent::Assistant { .. } => {
                *self.model_turns.entry(agent.into()).or_default() += 1;
            }
            HistoryEvent::ModelReminder { model, turn } => {
                self.model_reminders
                    .insert((agent.into(), model.clone()), *turn);
            }
            HistoryEvent::TitleChange { title, source } => {
                self.header.title = title.clone();
                self.header.title_source = *source;
            }
            HistoryEvent::Pruned { report, .. } if report.cache_break => {
                self.requests.remove(agent);
            }
            HistoryEvent::AgentRequest { checkpoint } => {
                self.requests.insert(agent.into(), *checkpoint.clone());
            }
            HistoryEvent::Usage { usage } => {
                self.totals.add(usage);
                self.agent_totals
                    .entry(agent.into())
                    .or_default()
                    .add(usage);
            }
            HistoryEvent::JobUpdate { job } => {
                if job.kind == "task"
                    && job.running()
                    && let Some(execution) = job
                        .agent_id
                        .as_ref()
                        .and_then(|id| self.executions.get_mut(id))
                {
                    execution.background = true;
                }
                self.jobs.insert(job.id.clone(), job.clone());
            }
            HistoryEvent::AgentStarted {
                agent,
                execution,
                task_call_id,
            } => {
                let launch = if execution.launch_id.is_empty() {
                    format!(
                        "{}:{}",
                        agent.agent_id,
                        task_call_id.as_deref().unwrap_or("legacy")
                    )
                } else {
                    execution.launch_id.clone()
                };
                if self.launches_seen.insert(launch.clone()) {
                    self.partial_reports.remove(&agent.agent_id);
                    self.finalizing.remove(&agent.agent_id);
                    let new_node = !self.executions.contains_key(&agent.agent_id);
                    let notice = crate::reporting::GrowthNotice {
                        launch_id: launch,
                        agent_id: agent.agent_id.clone(),
                        title: execution.title.clone(),
                        model: agent.model.clone(),
                        started_by: agent.parent_id.clone().unwrap_or_default(),
                        depth: crate::subtree::depth(self, &agent.agent_id),
                    };
                    let mut next = agent.parent_id.clone();
                    while let Some(parent) = next {
                        *self.descendant_launches.entry(parent.clone()).or_default() += 1;
                        if new_node
                            && self.settings.subagents == code_config::SubagentMode::Tree
                            && notice.started_by != parent
                        {
                            self.growth_pending
                                .entry(parent.clone())
                                .or_default()
                                .push(notice.clone());
                        }
                        next = self.agents.get(&parent).and_then(|a| a.parent_id.clone());
                    }
                }
                self.agents.insert(agent.agent_id.clone(), agent.clone());
                self.executions
                    .insert(agent.agent_id.clone(), execution.clone());
            }
            HistoryEvent::GuidanceSelected { selection } => {
                self.guidance_selections
                    .insert(selection.id.clone(), selection.clone());
            }
            HistoryEvent::Subscriptions { paths } => {
                self.subscriptions.insert(agent.into(), paths.clone());
            }
            HistoryEvent::SubscriptionChanged { change }
                if change.source.as_deref() == Some(agent) =>
            {
                if let Some(pending) = self.subscription_pending.get_mut(agent) {
                    pending.changes.remove(&change.path);
                }
            }
            HistoryEvent::SubscriptionChanged { change } => crate::subscriptions::apply_pending(
                self.subscription_pending.entry(agent.into()).or_default(),
                change.clone(),
            ),
            HistoryEvent::SubscriptionOverflow => {
                self.subscription_pending
                    .entry(agent.into())
                    .or_default()
                    .overflow = true;
            }
            HistoryEvent::SubscriptionNotice { .. } => {
                self.subscription_pending.remove(agent);
            }
            HistoryEvent::LocalCommand {
                command,
                exit_code,
                duration_ms,
                output_bytes,
            } => {
                self.local_commands
                    .entry(agent.into())
                    .or_default()
                    .push_back(crate::local_command::notice_text(
                        command,
                        *exit_code,
                        *duration_ms,
                        *output_bytes,
                    ));
            }
            HistoryEvent::AgentScope { scope } => {
                self.scopes.insert(agent.into(), scope.clone());
            }
            HistoryEvent::ScopeRequest { request } => {
                if agent == request.owner {
                    self.scope_owner_records
                        .insert(request.id.clone(), request.updated.clone());
                    if request.resolved_by.as_deref() == Some("user")
                        && !matches!(request.status.as_str(), "pending" | "applying")
                        && self.scope_resolution_notices.insert(request.id.clone())
                    {
                        self.mailbox
                            .entry(agent.into())
                            .or_default()
                            .push_back(format!(
                                "The user resolved scope request {} from {} for {}: {}.{}",
                                request.id,
                                request.requester,
                                request.path.display(),
                                request.status,
                                request
                                    .reason
                                    .as_ref()
                                    .map(|r| format!(" {r}"))
                                    .unwrap_or_default()
                            ));
                    }
                }
                self.scope_requests
                    .insert(request.id.clone(), request.clone());
            }
            HistoryEvent::AgentState { agent } => {
                self.agents.insert(agent.agent_id.clone(), agent.clone());
            }
            HistoryEvent::Goal { contract } => {
                self.contracts.insert(agent.into(), contract.clone());
                if agent == "root"
                    && let Some(run) = &mut self.header.goal
                {
                    run.contract = contract.clone();
                }
            }
            HistoryEvent::GoalState { state } if agent == "root" => {
                self.header.goal = Some(*state.clone());
            }
            HistoryEvent::Question { question } => {
                self.questions.insert(question.id.clone(), question.clone());
            }
            HistoryEvent::Answer { id, text } => {
                self.deliver_answer(id, text.clone(), None);
            }
            HistoryEvent::QuestionsAnswered { id, answers } => {
                self.deliver_answer(
                    id,
                    serde_json::to_string(answers).unwrap(),
                    Some(answers.clone()),
                );
            }
            HistoryEvent::QuestionForkAnswered {
                id,
                answers,
                summary,
            } => {
                self.deliver_answer(
                    id,
                    serde_json::json!({"answers":answers,"summary":summary}).to_string(),
                    Some(answers.clone()),
                );
            }
            HistoryEvent::QuestionDiscussed { id } => {
                if let Some(q) = self.questions.get_mut(id) {
                    q.answer = Some("discuss: waiting for the user's next message".into());
                    q.discussing = true;
                }
            }
            HistoryEvent::DiscussionResumed { asker } => {
                for q in self.questions.values_mut().filter(|q| q.asker == *asker) {
                    q.discussing = false;
                }
            }
            HistoryEvent::Message { from, to, text } if to == agent => {
                self.message_sequences
                    .insert((to.clone(), from.clone()), self.sequence);
                self.mailbox
                    .entry(to.clone())
                    .or_default()
                    .push_back(format!("Message from {from}: {text}"));
            }
            HistoryEvent::SettingChange { key, value } => {
                let mut settings = serde_json::to_value(&self.settings).unwrap();
                settings[key] = value.clone();
                if let Ok(settings) = serde_json::from_value(settings) {
                    self.settings = settings;
                    if key == "model"
                        && let Some(root) = self.agents.get_mut("root")
                    {
                        root.model = self.settings.model.clone();
                    }
                    // Capability policy is shared by the whole tree. Deliver at
                    // each agent's next model boundary, after its tool results.
                    let recipients =
                        if matches!(key.as_str(), "mode" | "safetyModel" | "mcps" | "exclusive") {
                            self.agents
                                .values()
                                .filter(|a| !a.hidden())
                                .map(|a| a.agent_id.clone())
                                .collect::<Vec<_>>()
                        } else {
                            vec!["root".into()]
                        };
                    for recipient in recipients {
                        self.mailbox
                            .entry(recipient)
                            .or_default()
                            .push_back(format!("Session setting {key} changed to {value}"));
                    }
                }
            }
            HistoryEvent::SystemReminder { text } => {
                if let Some(queue) = self.local_commands.get_mut(agent)
                    && let Some(index) = queue
                        .iter()
                        .position(|s| s == text || Some(s.as_str()) == crate::injection::body(text))
                {
                    queue.remove(index);
                }
                if let Some(queue) = self.mailbox.get_mut(agent)
                    && let Some(index) = queue
                        .iter()
                        .position(|s| s == text || Some(s.as_str()) == crate::injection::body(text))
                {
                    queue.remove(index);
                }
            }
            _ => {}
        }
    }
    pub fn change_settings(&mut self, changes: Vec<(String, Value)>) -> Result<Vec<EventEnvelope>> {
        let mut proposed = serde_json::to_value(&self.settings).map_err(bad)?;
        for (key, value) in &changes {
            if proposed.get(key).is_none() {
                return Err(bad("unknown session setting"));
            }
            proposed[key] = value.clone();
        }
        let _: SessionSettings = serde_json::from_value(proposed).map_err(bad)?;
        self.ensure_persisted()?;
        let _guard = Guard::acquire(&self.dir)?;
        self.verify()?;
        let mut envelopes = vec![];
        let mut bytes = vec![];
        for (key, value) in changes {
            self.sequence += 1;
            let envelope = EventEnvelope {
                version: 1,
                id: Uuid::new_v4().to_string(),
                session_id: self.header.id.clone(),
                agent_id: "root".into(),
                turn_id: self.turn_id.clone(),
                attempt_id: self.attempt_id.clone(),
                sequence: self.sequence,
                ts: now(),
                event: HistoryEvent::SettingChange { key, value },
            };
            serde_json::to_writer(&mut bytes, &envelope).map_err(bad)?;
            bytes.push(b'\n');
            envelopes.push(envelope);
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(context_path(&self.dir, "root", self.contexts["root"]))
            .map_err(bad)?;
        file.write_all(&bytes).map_err(bad)?;
        file.sync_all().map_err(bad)?;
        for envelope in &envelopes {
            self.metadata("root", &envelope.event);
            reduce(
                self.histories.entry("root".into()).or_default(),
                &envelope.event,
            );
        }
        write_json(&self.dir.join("settings.json"), &self.settings)?;
        if envelopes
            .iter()
            .any(|e| matches!(&e.event,HistoryEvent::SettingChange{key,..}if key=="model"))
            && let Some(root) = self.agents.get_mut("root")
        {
            root.model = self.settings.model.clone();
            write_json(&self.dir.join("agents/root/agent.json"), root)?;
        }
        Ok(envelopes)
    }
    pub fn update_agent(&mut self, agent: AgentRecord) -> Result<()> {
        if !self.agents.contains_key(&agent.agent_id) {
            return Err(bad("agent not found"));
        }
        if self.agents[&agent.agent_id].parent_id != agent.parent_id {
            return Err(bad("agent parent is immutable"));
        }
        self.ensure_persisted()?;
        let _guard = Guard::acquire(&self.dir)?;
        self.verify()?;
        write_json(
            &self
                .dir
                .join("agents")
                .join(&agent.agent_id)
                .join("agent.json"),
            &agent,
        )?;
        self.agents.insert(agent.agent_id.clone(), agent);
        Ok(())
    }
    /// Claim the bounded post-turn slots durably, before starting background I/O.
    pub fn title_turn_finished(&mut self) -> Result<Option<u64>> {
        self.ensure_persisted()?;
        let _guard = Guard::acquire(&self.dir)?;
        self.verify()?;
        self.header.title_turns = self.header.title_turns.saturating_add(1);
        let eligible = self.header.title_source != TitleSource::User
            && matches!(self.header.title_turns, 1 | 4 | 12);
        if eligible {
            self.header.title_generation = self.header.title_generation.saturating_add(1);
        }
        write_json(&self.dir.join("session.json"), &self.header)?;
        Ok(eligible.then_some(self.header.title_generation))
    }
    pub fn set_title(
        &mut self,
        title: String,
        source: TitleSource,
        generation: Option<u64>,
    ) -> Result<Option<EventEnvelope>> {
        if title.trim().is_empty()
            || title.chars().any(char::is_control)
            || (source == TitleSource::Auto && title.chars().count() > 60)
        {
            return Err(EngineError::bad(
                "title must contain readable text (auto titles: at most 60 characters)",
            ));
        }
        if source == TitleSource::Auto
            && (self.header.title_source == TitleSource::User
                || generation != Some(self.header.title_generation))
        {
            return Ok(None);
        }
        let envelope = self.append("root", HistoryEvent::TitleChange { title, source })?;
        // Replay of the durable event repairs an interrupted header write on resume.
        let _guard = Guard::acquire(&self.dir)?;
        self.verify()?;
        if source == TitleSource::User {
            self.header.title_generation = self.header.title_generation.saturating_add(1);
        }
        write_json(&self.dir.join("session.json"), &self.header)?;
        Ok(Some(envelope))
    }
    pub fn rotate(&mut self, agent: &str, events: Vec<HistoryEvent>) -> Result<Vec<EventEnvelope>> {
        self.ensure_persisted()?;
        let _guard = Guard::acquire(&self.dir)?;
        self.verify()?;
        let n = self
            .contexts
            .get(agent)
            .copied()
            .ok_or_else(|| bad("agent missing"))?
            + 1;
        let path = context_path(&self.dir, agent, n);
        let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(bad)?;
        let mut envelopes = vec![];
        for mut event in events {
            self.sequence += 1;
            let event_id = Uuid::new_v4().to_string();
            crate::injection::canonicalize_fresh(&mut event, &event_id);
            let envelope = EventEnvelope {
                version: 1,
                id: event_id,
                session_id: self.header.id.clone(),
                agent_id: agent.into(),
                turn_id: self.activities.get(agent).map(|a| a.0.clone()),
                attempt_id: None,
                sequence: self.sequence,
                ts: now(),
                event,
            };
            serde_json::to_writer(&mut temp, &envelope).map_err(bad)?;
            temp.write_all(b"\n").map_err(bad)?;
            envelopes.push(envelope);
        }
        temp.as_file().sync_all().map_err(bad)?;
        temp.persist_noclobber(path).map_err(bad)?;
        self.contexts.insert(agent.into(), n);
        self.histories.insert(agent.into(), vec![]);
        for envelope in &envelopes {
            self.metadata(agent, &envelope.event);
            self.agent_activity
                .insert(agent.into(), envelope.ts.clone());
            if matches!(envelope.event, HistoryEvent::AgentState { .. }) {
                self.agent_updated.insert(agent.into(), envelope.ts.clone());
            }
            reduce(self.histories.get_mut(agent).unwrap(), &envelope.event);
        }
        Ok(envelopes)
    }
    pub fn register(&mut self, agent: AgentRecord) -> Result<()> {
        valid_id(&agent.agent_id)?;
        if self.agents.contains_key(&agent.agent_id) {
            return Err(bad("agent already exists"));
        }
        if agent
            .parent_id
            .as_ref()
            .is_some_and(|p| !self.agents.contains_key(p))
        {
            return Err(bad("parent is not registered"));
        }
        self.ensure_persisted()?;
        let _guard = Guard::acquire(&self.dir)?;
        self.verify()?;
        let path = self.dir.join("agents").join(&agent.agent_id);
        fs::create_dir_all(path.join("contexts")).map_err(bad)?;
        write_json(&path.join("agent.json"), &agent)?;
        File::create(context_path(&self.dir, &agent.agent_id, 1)).map_err(bad)?;
        self.contexts.insert(agent.agent_id.clone(), 1);
        self.histories.insert(agent.agent_id.clone(), vec![]);
        self.agents.insert(agent.agent_id.clone(), agent);
        Ok(())
    }
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        if !self.persisted {
            self.closed = true;
            self.read_only = true;
            return Ok(());
        }
        let _guard = Guard::acquire(&self.dir)?;
        if status(&self.dir, Some(&self.token), self.read_only)?.owned {
            fs::remove_file(self.dir.join("lock.json")).map_err(bad)?;
        }
        self.read_only = true;
        self.closed = true;
        Ok(())
    }
}
fn retained_parent(agents: &BTreeMap<String, AgentRecord>, parent: &str, boundary: &str) -> bool {
    agents.get(parent).is_some_and(|a| {
        a.agent_id == "root"
            || (a.created_at.as_str() <= boundary
                && a.parent_id
                    .as_ref()
                    .is_none_or(|p| retained_parent(agents, p, boundary)))
    })
}
fn copy_assets(source: &Path, dest: &Path) -> Result<()> {
    for e in fs::read_dir(source).map_err(bad)? {
        let e = e.map_err(bad)?;
        let ty = e.file_type().map_err(bad)?;
        if ty.is_file() {
            fs::copy(e.path(), dest.join(e.file_name())).map_err(bad)?;
        } else if ty.is_dir() {
            let path = dest.join(e.file_name());
            fs::create_dir(&path).map_err(bad)?;
            copy_assets(&e.path(), &path)?;
        }
    }
    Ok(())
}
impl Drop for Journal {
    fn drop(&mut self) {
        if self.release_on_drop {
            let _ = self.close();
        }
    }
}

/// Best-effort account-wide cost recovery. Forked call IDs are deduplicated by the cache.
pub(crate) fn usage_records(data: &Path) -> Vec<(String, UsageRecord)> {
    let mut records = Vec::new();
    for entry in fs::read_dir(data.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if let Ok(contexts) = read_contexts(&entry.path()) {
            for events in contexts.values().flat_map(|c| c.values()) {
                for envelope in events {
                    if let HistoryEvent::Usage { usage } = &envelope.event {
                        records.push((envelope.ts.clone(), usage.clone()));
                    }
                }
            }
        }
    }
    records
}
