//! Durable write ownership. Scope approval never substitutes for the permission gate.
use crate::{
    Control, Session, State,
    schema::*,
    sessions,
    tools::{CommitBarrier, PermissionGate, PreparedOperation, ToolBox},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentScope {
    pub paths: Vec<PathBuf>,
    /// Only explicit scopes reserve ownership against siblings. Omitted task.scope inherits access.
    pub exclusive: bool,
    #[serde(default)]
    pub grants: BTreeMap<PathBuf, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeRequest {
    pub id: String,
    pub requester: String,
    pub owner: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub call: ToolCall,
    pub expected_hash: Option<String>,
    pub job_id: Option<String>,
    pub parent_call_id: Option<String>,
    pub status: String,
    /// Agent id, or "user" for a frontend decision. Older records omit this.
    #[serde(default)]
    pub resolved_by: Option<String>,
    /// Only an owner escalation requests a frontend decision.
    #[serde(default)]
    pub escalation: Option<String>,
    pub wait: bool,
    pub reason: Option<String>,
    pub output: Option<String>,
    pub created: String,
    pub updated: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ScopeDecision {
    Apply,
    Reject { reason: String },
    Grant { scope: Vec<PathBuf> },
    Escalate { why: String },
}
pub(crate) struct ScopePermit {
    pub request_id: String,
    pub path: PathBuf,
}
tokio::task_local! { static SCOPE_EXECUTOR: String; }
pub(crate) fn execution_owner(author: &str) -> String {
    SCOPE_EXECUTOR
        .try_with(Clone::clone)
        .unwrap_or_else(|_| author.into())
}
fn active(j: &sessions::Journal, id: &str) -> bool {
    id == "root"
        || j.agents.get(id).is_some_and(|a| {
            !matches!(
                a.status.as_str(),
                "done" | "failed" | "roadblocked" | "killed" | "interrupted"
            )
        })
}
fn ancestor(j: &sessions::Journal, parent: &str, child: &str) -> bool {
    let mut next = Some(child);
    while let Some(id) = next {
        if id == parent {
            return true;
        }
        next = j.agents.get(id).and_then(|a| a.parent_id.as_deref());
    }
    false
}
fn depth(j: &sessions::Journal, id: &str) -> usize {
    let mut n = 0;
    let mut next = j.agents.get(id).and_then(|a| a.parent_id.as_deref());
    while let Some(id) = next {
        n += 1;
        next = j.agents.get(id).and_then(|a| a.parent_id.as_deref());
    }
    n
}
pub(crate) fn effective(j: &sessions::Journal, id: &str) -> AgentScope {
    j.scopes.get(id).cloned().unwrap_or_else(|| AgentScope {
        paths: vec![j.header.cwd.clone()],
        exclusive: id == "root",
        grants: BTreeMap::new(),
    })
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
fn shared(root: &Path, path: &Path, patterns: &[String]) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim_end_matches('/');
        if pattern.is_empty() {
            return false;
        }
        let glob = globset::Glob::new(pattern)
            .ok()
            .map(|g| g.compile_matcher());
        relative.ancestors().any(|part| {
            glob.as_ref().is_some_and(|g| {
                g.is_match(part) || part.file_name().is_some_and(|n| g.is_match(Path::new(n)))
            })
        })
    })
}
/// Root/ancestor writes are allowed through active descendants, but lateral writes request ownership.
fn owner(
    j: &sessions::Journal,
    requester: &str,
    path: &Path,
    patterns: &[String],
) -> Option<String> {
    if requester == "root" || shared(&j.header.cwd, path, patterns) {
        return None;
    }
    let holder = j
        .scopes
        .iter()
        .filter(|(id, scope)| {
            active(j, id) && scope.exclusive && scope.paths.iter().any(|p| path.starts_with(p))
        })
        .max_by_key(|(id, _)| depth(j, id))
        .map(|(id, _)| id.as_str())
        .unwrap_or("root");
    if effective(j, requester)
        .grants
        .iter()
        .any(|(p, issuer)| path.starts_with(p) && (issuer == holder || ancestor(j, holder, issuer)))
    {
        return None;
    }
    if holder != requester && !ancestor(j, requester, holder) && !ancestor(j, holder, requester) {
        return Some(holder.into());
    }
    if effective(j, requester)
        .paths
        .iter()
        .any(|p| path.starts_with(p))
    {
        return None;
    }
    let mut next = j.agents.get(requester).and_then(|a| a.parent_id.as_deref());
    while let Some(id) = next {
        if active(j, id) && effective(j, id).paths.iter().any(|p| path.starts_with(p)) {
            return Some(id.into());
        }
        next = j.agents.get(id).and_then(|a| a.parent_id.as_deref());
    }
    Some("root".into())
}
fn paths(cwd: &Path, values: &Value) -> Result<Vec<PathBuf>, EngineError> {
    let list = values
        .as_array()
        .ok_or_else(|| EngineError::bad("scope must be an array of directory paths"))?;
    if list.len() > 32 {
        return Err(EngineError::bad("scope is capped at 32 paths"));
    }
    let resolver = crate::paths::Resolver::new(cwd.into());
    let mut result = BTreeSet::new();
    for value in list {
        let raw = value
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| EngineError::bad("scope paths must be nonempty strings"))?;
        let path = resolver
            .resolve(raw)
            .map_err(|e| EngineError::bad(e.to_string()))?;
        if path.exists() && !path.is_dir() {
            return Err(EngineError::bad("scope paths must be directories"));
        }
        resolver
            .validate(&path)
            .map_err(|e| EngineError::bad(e.to_string()))?;
        result.insert(path);
    }
    Ok(result.into_iter().collect())
}
/// Called under the same journal lock as sibling registration/reactivation.
pub(crate) fn task_scope(
    j: &sessions::Journal,
    parent: &str,
    id: &str,
    cwd: &Path,
    args: &Value,
) -> Result<AgentScope, EngineError> {
    let candidate = if let Some(value) = args.get("scope") {
        AgentScope {
            paths: paths(cwd, value)?,
            exclusive: true,
            grants: BTreeMap::new(),
        }
    } else {
        j.scopes.get(id).cloned().unwrap_or_else(|| AgentScope {
            paths: effective(j, parent).paths,
            exclusive: false,
            grants: effective(j, parent).grants,
        })
    };
    let parent_scope = effective(j, parent);
    for path in &candidate.paths {
        if !parent_scope.paths.iter().any(|p| path.starts_with(p)) {
            return Err(EngineError::bad(format!(
                "scope {} is outside the parent's scope",
                path.display()
            )));
        }
        if candidate.exclusive {
            for (other, scope) in &j.scopes {
                if other != id
                    && active(j, other)
                    && scope.exclusive
                    && !ancestor(j, other, parent)
                    && !ancestor(j, parent, other)
                    && scope.paths.iter().any(|p| overlaps(p, path))
                {
                    return Err(EngineError::bad(format!(
                        "scope {} overlaps active owner {other}; scope cannot be re-acquired",
                        path.display()
                    )));
                }
                if other != id
                    && active(j, other)
                    && j.agents.get(other).and_then(|a| a.parent_id.as_deref()) == Some(parent)
                    && scope.exclusive
                    && scope.paths.iter().any(|p| overlaps(p, path))
                {
                    return Err(EngineError::bad(format!(
                        "scope {} overlaps active sibling {other}",
                        path.display()
                    )));
                }
            }
        }
    }
    Ok(candidate)
}
impl Control {
    fn save_scope_request(&self, mut request: ScopeRequest) -> Result<ScopeRequest, EngineError> {
        let patterns = self.config.lock().unwrap().config.shared_paths.clone();
        let envelopes = {
            let mut j = self.journal.lock().unwrap();
            if j.scope_requests
                .get(&request.id)
                .is_some_and(|old| old.updated != request.updated)
            {
                return Err(EngineError::bad("scope request changed; re-read and retry"));
            }
            if request.status == "pending" && !active(&j, &request.owner) {
                if j.agents
                    .get(&request.owner)
                    .is_some_and(|a| matches!(a.status.as_str(), "killed" | "interrupted"))
                {
                    request.status = "unavailable".into();
                    request.reason = Some("owner unavailable".into());
                } else {
                    let old = request.owner.clone();
                    let mut parent = j
                        .agents
                        .get(&old)
                        .and_then(|a| a.parent_id.clone())
                        .unwrap_or_else(|| "root".into());
                    while !active(&j, &parent) {
                        parent = j
                            .agents
                            .get(&parent)
                            .and_then(|a| a.parent_id.clone())
                            .unwrap_or_else(|| "root".into());
                    }
                    request.escalation = None;
                    request.owner =
                        owner(&j, &request.requester, &request.path, &patterns).unwrap_or(parent);
                    request.reason = Some(format!(
                        "Previous owner {old} finished; request rerouted to {}",
                        request.owner
                    ));
                }
            }
            // An escalated request waits on the user, not on its owning agent.
            if request.wait
                && request.escalation.is_none()
                && matches!(request.status.as_str(), "pending" | "applying")
            {
                refuse_cycle(&j, &request.requester, &request.owner, Some(&request.id))?;
            }
            request.updated = sessions::now();
            let event = HistoryEvent::ScopeRequest {
                request: request.clone(),
            };
            let mut envelopes = vec![j.append(&request.requester, event.clone())?];
            if request.owner != request.requester {
                envelopes.push(j.append(&request.owner, event)?);
            }
            envelopes
        };
        if !matches!(request.status.as_str(), "pending" | "applying") {
            self.scope_serviced.lock().unwrap().remove(&request.id);
        }
        let decision = envelopes.first().and_then(crate::decisions::automatic);
        for envelope in envelopes {
            self.emit(EngineEvent::Committed { envelope });
        }
        if let Some(record) = decision {
            self.record_decision(record)?;
        }
        self.emit(EngineEvent::ScopeRequestUpdate {
            request: request.clone(),
        });
        if !self.scope_service_needed(&request.owner) {
            self.scheduler.release_owner(&request.owner);
        }
        self.changed.notify_waiters();
        Ok(request)
    }
    pub(crate) async fn request_scope(
        &self,
        op: &PreparedOperation,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, EngineError> {
        if !matches!(op.call.name.as_str(), "write" | "edit") {
            return Ok(None);
        }
        let Some(path) = &op.target else {
            return Ok(None);
        };
        let config = self.config.lock().unwrap().config.clone();
        let request = {
            let j = self.journal.lock().unwrap();
            let Some(owner) = owner(&j, &op.agent_id, path, &config.shared_paths) else {
                return Ok(None);
            };
            if j.scope_requests
                .values()
                .filter(|r| matches!(r.status.as_str(), "pending" | "applying"))
                .count()
                >= 128
            {
                return Err(EngineError::bad(
                    "128 scope requests are already pending; resolve one before requesting another",
                ));
            }
            ScopeRequest {
                id: uuid::Uuid::new_v4().to_string(),
                requester: op.agent_id.clone(),
                owner,
                path: path.clone(),
                cwd: op.cwd.clone(),
                call: op.call.clone(),
                expected_hash: op.expected_digest(),
                job_id: op.job_id.clone(),
                parent_call_id: op.parent_call_id.clone(),
                status: "pending".into(),
                resolved_by: None,
                escalation: None,
                wait: op.call.arguments["wait"] == true,
                reason: None,
                output: None,
                created: sessions::now(),
                updated: sessions::now(),
            }
        };
        if request.wait {
            refuse_cycle(
                &self.journal.lock().unwrap(),
                &request.requester,
                &request.owner,
                Some(&request.id),
            )?;
        }
        let request = self.save_scope_request(request)?;
        if request.status != "pending" {
            return Ok(Some(json!({"requestId":request.id,"owner":request.owner,"status":request.status,"applied":false,"reason":request.reason}).to_string()));
        }
        self.scheduler.prioritize_owner(&request.owner);
        self.message(&request.requester,&request.owner,format!("Write scope request {} from {} for {}. Use scope.requests() and scope.resolve(requestId=\"{}\", decision=\"apply\" | \"reject\" | \"grant\", reason=..., scope=[...]). The write is pending; scope approval still passes the permission gate. Answer as the owner; only escalate with why when you need the user to decide.",request.id,request.requester,request.path.display(),request.id))?;
        if request.wait {
            return self
                .wait_scope(&request.requester, &request.id, cancel)
                .await
                .map(Some);
        }
        Ok(Some(json!({"requestId":request.id,"owner":request.owner,"status":"pending","applied":false,"message":"Continue working or call scope.wait(requestId=...) to wait."}).to_string()))
    }
    async fn wait_scope(
        &self,
        agent: &str,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<String, EngineError> {
        let mut request = {
            let j = self.journal.lock().unwrap();
            let request = j
                .scope_requests
                .get(id)
                .cloned()
                .ok_or_else(|| EngineError::bad("scope request not found"))?;
            if request.requester != agent {
                return Err(EngineError::bad("only the requester may wait"));
            }
            if request.escalation.is_none()
                && matches!(request.status.as_str(), "pending" | "applying")
            {
                refuse_cycle(&j, agent, &request.owner, Some(id))?;
            }
            request
        };
        if matches!(request.status.as_str(), "pending" | "applying") && !request.wait {
            request.wait = true;
            self.save_scope_request(request)?;
        }
        self.scheduler.park(agent);
        self.agent_status(agent, "waiting")?;
        let result = async {
            loop {
                let changed = self.changed.notified(); tokio::pin!(changed); changed.as_mut().enable();
                self.check_owner()?;
                let request = self.journal.lock().unwrap().scope_requests.get(id).cloned().ok_or_else(||EngineError::bad("scope request not found"))?;
                if request.requester != agent { return Err(EngineError::bad("only the requester may wait")); }
                if !matches!(request.status.as_str(),"pending"|"applying") { return Ok(serde_json::to_string(&request).unwrap()); }
                tokio::select! { _=cancel.cancelled()=>return Err(EngineError::cancelled()), _=&mut changed=>{} }
            }
        }.await;
        if result.is_err() {
            let pending = self
                .journal
                .lock()
                .unwrap()
                .scope_requests
                .get(id)
                .filter(|r| matches!(r.status.as_str(), "pending" | "applying"))
                .cloned();
            if let Some(mut request) = pending {
                request.wait = false;
                let _ = self.save_scope_request(request);
            }
        }
        self.scheduler.wake(agent);
        let acquired = self.scheduler.acquire(agent, cancel).await;
        if result.is_ok() {
            acquired?;
        }
        self.agent_status(agent, "running")?;
        result
    }
}
impl Session {
    pub fn agent_scopes(&self) -> BTreeMap<String, AgentScope> {
        let j = self.control.journal.lock().unwrap();
        let mut scopes = j.scopes.clone();
        scopes.insert("root".into(), effective(&j, "root"));
        scopes
    }
    pub fn pending_scope_requests(&self) -> Vec<ScopeRequest> {
        self.control
            .journal
            .lock()
            .unwrap()
            .scope_requests
            .values()
            .filter(|r| matches!(r.status.as_str(), "pending" | "applying"))
            .cloned()
            .collect()
    }
    pub async fn resolve_scope_request(
        &self,
        id: &str,
        decision: ScopeDecision,
    ) -> Result<ScopeRequest, EngineError> {
        resolve(
            self.control.clone(),
            self.gate.clone(),
            None,
            id,
            decision,
            &CancellationToken::new(),
            Arc::new(CommitBarrier::default()),
        )
        .await
    }
}
async fn resolve(
    control: Arc<Control>,
    gate: Arc<dyn PermissionGate>,
    actor: Option<&str>,
    id: &str,
    decision: ScopeDecision,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
) -> Result<ScopeRequest, EngineError> {
    let mut request = control
        .journal
        .lock()
        .unwrap()
        .scope_requests
        .get(id)
        .cloned()
        .ok_or_else(|| EngineError::bad("scope request not found"))?;
    if request.status != "pending" {
        return Err(EngineError::bad("scope request is no longer pending"));
    }
    if actor.is_some_and(|a| a != request.owner) {
        return Err(EngineError::bad(
            "only the scope owner may resolve this request",
        ));
    }
    if actor.is_some() && request.escalation.is_some() {
        return Err(EngineError::bad(
            "request escalated to the user; await their decision",
        ));
    }
    if actor.is_none() && request.escalation.is_none() {
        return Err(EngineError::bad(
            "only an explicitly escalated request may be answered by the user",
        ));
    }
    control.check_owner()?;
    if let ScopeDecision::Escalate { why } = &decision {
        if actor.is_none() || why.trim().is_empty() || why.chars().count() > 4096 {
            return Err(EngineError::bad(
                "owner escalation requires why (1–4096 characters)",
            ));
        }
        request.escalation = Some(why.trim().into());
        return control.save_scope_request(request);
    }
    request.resolved_by = Some(actor.unwrap_or("user").into());
    let mut applied = None;
    match decision {
        ScopeDecision::Escalate { .. } => unreachable!(),
        ScopeDecision::Reject { reason } => {
            if reason.trim().is_empty() || reason.chars().count() > 4096 {
                return Err(EngineError::bad(
                    "reject requires a reason of 1–4096 characters",
                ));
            }
            request.status = "rejected".into();
            request.reason = Some(reason);
        }
        ScopeDecision::Grant { mut scope } => {
            if scope.is_empty() {
                scope.push(request.path.parent().unwrap_or(&request.cwd).to_path_buf());
            }
            let config = control.config.lock().unwrap().config.clone();
            let mut j = control.journal.lock().unwrap();
            if owner(&j, &request.requester, &request.path, &config.shared_paths).as_deref()
                != Some(&request.owner)
            {
                return Err(EngineError::bad(
                    "scope ownership changed; re-read the request",
                ));
            }
            let resolver = crate::paths::Resolver::new(request.cwd.clone());
            let mut assignment = effective(&j, &request.requester);
            let owner_scope = effective(&j, &request.owner);
            if scope.is_empty() || scope.len() > 32 {
                return Err(EngineError::bad("grant requires 1–32 directory paths"));
            }
            for raw in scope {
                let path = resolver
                    .resolve(&raw.to_string_lossy())
                    .map_err(|e| EngineError::bad(e.to_string()))?;
                if path.exists() && !path.is_dir() {
                    return Err(EngineError::bad("grant scope must contain directories"));
                }
                if !owner_scope.paths.iter().any(|p| path.starts_with(p)) {
                    return Err(EngineError::bad("grant must be inside the owner's scope"));
                }
                if j.scopes.iter().any(|(id, s)| {
                    active(&j, id)
                        && s.exclusive
                        && !ancestor(&j, id, &request.owner)
                        && !ancestor(&j, &request.owner, id)
                        && id != &request.requester
                        && s.paths.iter().any(|p| overlaps(p, &path))
                }) {
                    return Err(EngineError::bad("grant overlaps another active owner"));
                }
                assignment.grants.insert(path, request.owner.clone());
            }
            assignment.paths.sort();
            assignment.paths.dedup();
            if !assignment
                .grants
                .keys()
                .any(|p| request.path.starts_with(p))
            {
                return Err(EngineError::bad("grant must cover the requested file"));
            }
            let envelope = j.append(
                &request.requester,
                HistoryEvent::AgentScope { scope: assignment },
            )?;
            drop(j);
            control.emit(EngineEvent::Committed { envelope });
            request.status = "granted".into();
            request.output=Some("Scope granted. Re-read the file and retry the write/edit through its normal permission gate.".into());
        }
        ScopeDecision::Apply => {
            let config = control.config.lock().unwrap().config.clone();
            {
                let j = control.journal.lock().unwrap();
                let current = owner(&j, &request.requester, &request.path, &config.shared_paths);
                if current.as_deref().is_some_and(|o| o != request.owner) {
                    return Err(EngineError::bad(
                        "scope ownership changed; re-read the request",
                    ));
                }
            }
            let actual = std::fs::read(&request.path).map(|b| crate::context::digest(&b));
            let actual = match actual {
                Ok(hash) => Some(hash),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(EngineError::bad(e.to_string())),
            };
            if actual != request.expected_hash {
                request.status = "stale".into();
                request.reason = Some(
                    "File changed while awaiting the owner. Re-read and submit a fresh edit."
                        .into(),
                );
            } else {
                request.status = "applying".into();
                request = control.save_scope_request(request)?;
                let mut tools = ToolBox::new(request.cwd.clone(), gate.clone());
                tools.control = Arc::downgrade(&control);
                tools
                    .prepare_scope_apply(&request)
                    .map_err(|e| EngineError::bad(e.to_string()))?;
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let relay_control = control.clone();
                let agent = request.requester.clone();
                let relay = tokio::spawn(async move {
                    while let Some(event) = rx.recv().await {
                        relay_control.emit_for(&agent, event);
                    }
                });
                let token = cancel.child_token();
                control
                    .scope_applies
                    .lock()
                    .unwrap()
                    .insert(request.id.clone(), (barrier.clone(), token.clone()));
                let mut output = SCOPE_EXECUTOR
                    .scope(
                        request.owner.clone(),
                        tools.execute(request.call.clone(), token, barrier, &tx),
                    )
                    .await;
                output.output.push_str(&control.check_reviewer_guidance(
                    Some(&request.call.id),
                    None,
                    true,
                ));
                control.scope_applies.lock().unwrap().remove(&request.id);
                let current = control
                    .journal
                    .lock()
                    .unwrap()
                    .scope_requests
                    .get(&request.id)
                    .cloned();
                if let Some(current) = current.filter(|r| r.updated != request.updated) {
                    if output.is_error {
                        drop(tx);
                        let _ = relay.await;
                        return Ok(current);
                    }
                    request = current;
                    request.reason = Some(
                        "Write committed before its owner finished or became unavailable.".into(),
                    );
                }
                drop(tx);
                let _ = relay.await;
                let stale = output.is_error
                    && (output.output.contains("scope request changed")
                        || output.output.contains("file changed")
                        || match std::fs::read(&request.path) {
                            Ok(bytes) => {
                                Some(crate::context::digest(&bytes)) != request.expected_hash
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                request.expected_hash.is_some()
                            }
                            Err(_) => false,
                        });
                request.status = if stale {
                    "stale"
                } else if output.is_error {
                    "failed"
                } else {
                    "applied"
                }
                .into();
                if stale {
                    request.reason=Some("File changed while applying this request; re-read and submit a fresh edit.".into());
                }
                request.output = Some(output.output.clone());
                if !output.is_error {
                    applied = Some((tools, output));
                }
            }
        }
    }
    let mut request = control.save_scope_request(request)?;
    if let Some((tools, mut output)) = applied {
        crate::guidance_selector::after_scope_apply(
            control.clone(),
            gate,
            tools,
            &request,
            &mut output,
            cancel,
        )
        .await;
        if request.output.as_deref() != Some(&output.output) {
            request.output = Some(output.output);
            request = control.save_scope_request(request)?;
        }
    }
    control.message(
        &request.owner,
        &request.requester,
        format!(
            "Scope request {} {}: {}",
            request.id,
            request.status,
            request
                .reason
                .as_deref()
                .or(request.output.as_deref())
                .unwrap_or("")
        ),
    )?;
    Ok(request)
}
pub(crate) fn definition() -> crate::tools::ToolDefinition {
    crate::tools::ToolDefinition {name:"scope".into(),description:"Inspect write scopes and pending requests. Out-of-scope write/edit returns a durable request without waiting by default. Owner resolve: apply, reject with reason, grant directory scope (omitted/empty defaults to the changed file directory), or escalate with why to ask the user. No user dialog unless escalated. Apply revalidates the edit and still requires normal permissions; grant lets requester re-read/retry. Only requester may wait. Bash is unrestricted by scopes.".into(),schema:json!({"type":"object","properties":{"action":{"type":"string","enum":["show","requests","resolve","wait","subscribe"]},"requestId":{"type":"string"},"decision":{"type":"string","enum":["apply","reject","grant","escalate"]},"reason":{"type":"string"},"why":{"type":"string"},"scope":{"type":"array","items":{"type":"string"}},"subscribe":{"type":"array","items":{"type":"string"}}},"required":["action"],"additionalProperties":false})}
}
pub(crate) async fn execute(
    state: &State,
    args: &Value,
    cancel: &CancellationToken,
    barrier: Arc<CommitBarrier>,
) -> Result<String, EngineError> {
    let result = match args["action"].as_str() {
        Some("show") => {
            let j = state.control.journal.lock().unwrap();
            json!({"agentId":state.agent_id,"scope":effective(&j,&state.agent_id),"agents":j.scopes,"requests":j.scope_requests.values().filter(|r|r.owner==state.agent_id||r.requester==state.agent_id).collect::<Vec<_>>()})
        }
        Some("requests") => {
            let j = state.control.journal.lock().unwrap();
            json!(j.scope_requests.values().collect::<Vec<_>>())
        }
        Some("subscribe") => {
            let paths: Vec<PathBuf> = serde_json::from_value(args["subscribe"].clone())
                .map_err(|e| EngineError::bad(e.to_string()))?;
            for path in &paths {
                let path = crate::paths::Resolver::new(state.cwd.clone())
                    .resolve(&path.to_string_lossy())
                    .map_err(|e| EngineError::bad(e.to_string()))?;
                if !path.starts_with(&state.cwd) {
                    let call = ToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: "read".into(),
                        arguments: json!({"path":path}),
                        complete: true,
                    };
                    state
                        .gate
                        .authorize(&state.metadata_operation(call, Some(path)), cancel)
                        .await
                        .map_err(|e| EngineError::bad(e.to_string()))?;
                }
            }
            json!(state.control.set_subscriptions(&state.agent_id, &paths)?)
        }
        Some("wait") => {
            return state
                .control
                .wait_scope(
                    &state.agent_id,
                    args["requestId"]
                        .as_str()
                        .ok_or_else(|| EngineError::bad("requestId required"))?,
                    cancel,
                )
                .await;
        }
        Some("resolve") => {
            let decision = match args["decision"].as_str() {
                Some("apply") => ScopeDecision::Apply,
                Some("reject") => ScopeDecision::Reject {
                    reason: args["reason"].as_str().unwrap_or("").into(),
                },
                Some("grant") => ScopeDecision::Grant {
                    scope: serde_json::from_value(
                        args.get("scope").cloned().unwrap_or_else(|| json!([])),
                    )
                    .map_err(|e| EngineError::bad(e.to_string()))?,
                },
                Some("escalate") => ScopeDecision::Escalate {
                    why: args["why"].as_str().unwrap_or("").into(),
                },
                _ => {
                    return Err(EngineError::bad(
                        "decision must be apply, reject, grant or escalate",
                    ));
                }
            };
            json!(
                resolve(
                    state.control.clone(),
                    state.gate.clone(),
                    Some(&state.agent_id),
                    args["requestId"]
                        .as_str()
                        .ok_or_else(|| EngineError::bad("requestId required"))?,
                    decision,
                    cancel,
                    barrier
                )
                .await?
            )
        }
        _ => return Err(EngineError::bad("unknown scope action")),
    };
    Ok(result.to_string())
}

fn refuse_cycle(
    j: &sessions::Journal,
    requester: &str,
    owner: &str,
    except: Option<&str>,
) -> Result<(), EngineError> {
    let mut todo = vec![(owner.to_owned(), Vec::<String>::new(), None::<ScopeRequest>)];
    let mut seen = BTreeSet::new();
    while let Some((agent, chain, request)) = todo.pop() {
        if agent == requester {
            let recovery = match request {
                Some(r) if r.status == "applying" => format!(
                    "Scope request {} is already applying; let that operation finish before waiting for {}.",
                    r.id, owner
                ),
                Some(r) if r.owner == requester => format!(
                    "Answer request {} first: scope {{action:\"resolve\", requestId:\"{}\", decision:\"apply\"|\"reject\"|\"grant\"}} (reject needs reason; grant needs scope). Then retry the wait.",
                    r.id, r.id
                ),
                Some(r) => format!(
                    "Ask {} to answer request {} with apply/reject/grant first. Continue without waiting until it is resolved.",
                    r.owner, r.id
                ),
                None => "Continue without waiting. For an out-of-scope write/edit, use wait:false so the owner can receive and answer the request.".into(),
            };
            return Err(EngineError::bad(format!(
                "Wait cycle refused: {requester} would wait for {owner}; {}. Neither can progress while both wait. {recovery}",
                if chain.is_empty() {
                    "an agent cannot wait for itself".into()
                } else {
                    chain.join("; ")
                }
            )));
        }
        if !seen.insert(agent.clone()) {
            continue;
        }
        for (_, to) in j.wait_edges.values().filter(|(from, _)| from == &agent) {
            let mut next = chain.clone();
            next.push(format!("{agent} is already waiting for {to}"));
            todo.push((to.clone(), next, request.clone()));
        }
        for r in j.scope_requests.values().filter(|r| {
            Some(r.id.as_str()) != except
                && r.wait
                && r.escalation.is_none()
                && matches!(r.status.as_str(), "pending" | "applying")
                && r.requester == agent
        }) {
            let mut next = chain.clone();
            next.push(format!(
                "{agent} is waiting for {} to answer scope request {} for {} ({})",
                r.owner,
                r.id,
                r.path.display(),
                r.status
            ));
            let recovery = if r.owner == requester || request.is_none() {
                Some(r.clone())
            } else {
                request.clone()
            };
            todo.push((r.owner.clone(), next, recovery));
        }
    }
    Ok(())
}
impl Control {
    pub(crate) fn scope_service_needed(&self, agent: &str) -> bool {
        self.journal
            .lock()
            .unwrap()
            .scope_requests
            .values()
            .any(|r| {
                r.owner == agent
                    && r.escalation.is_none()
                    && matches!(r.status.as_str(), "pending" | "applying")
                    && !self.scope_serviced.lock().unwrap().contains(&r.id)
            })
    }
    pub(crate) fn scope_owner_ended(&self, id: &str, status: &str) -> Result<(), EngineError> {
        if !matches!(
            status,
            "done" | "failed" | "roadblocked" | "killed" | "interrupted"
        ) {
            return Ok(());
        }
        let cancelled = matches!(status, "killed" | "interrupted");
        let requests = self
            .journal
            .lock()
            .unwrap()
            .scope_requests
            .values()
            .filter(|r| matches!(r.status.as_str(), "pending" | "applying") && r.owner == id)
            .cloned()
            .collect::<Vec<_>>();
        for mut request in requests {
            if let Some((barrier, token)) = self.scope_applies.lock().unwrap().remove(&request.id) {
                barrier.revoke();
                token.cancel();
            }
            if cancelled {
                request.status = "unavailable".into();
                request.reason = Some("owner unavailable".into());
                let request = self.save_scope_request(request)?;
                self.message(
                    id,
                    &request.requester,
                    format!(
                        "Scope request {}: {}",
                        request.id,
                        request.reason.as_deref().unwrap_or("owner unavailable")
                    ),
                )?;
            } else if request.owner == id {
                let parent = {
                    let j = self.journal.lock().unwrap();
                    let mut next = j.agents.get(id).and_then(|a| a.parent_id.clone());
                    let mut found = "root".to_owned();
                    while let Some(parent) = next {
                        if active(&j, &parent)
                            && effective(&j, &parent)
                                .paths
                                .iter()
                                .any(|p| request.path.starts_with(p))
                        {
                            found = parent;
                            break;
                        }
                        next = j.agents.get(&parent).and_then(|a| a.parent_id.clone());
                    }
                    found
                };
                self.scope_serviced.lock().unwrap().remove(&request.id);
                request.owner = parent;
                request.escalation = None;
                request.status = "pending".into();
                request.reason = Some(format!(
                    "Previous owner {id} finished; scope reverted to {}",
                    request.owner
                ));
                let request = self.save_scope_request(request)?;
                self.scheduler.prioritize_owner(&request.owner);
                self.message(id,&request.owner,format!("Pending scope request {} moved to you when {id} finished. Use scope.requests() and scope.resolve(requestId=..., decision=...).",request.id))?;
            }
        }
        self.scheduler.release_owner(id);
        Ok(())
    }
    pub(crate) fn restore_scopes(&self) -> Result<(), EngineError> {
        let requests = self
            .journal
            .lock()
            .unwrap()
            .scope_requests
            .values()
            .filter(|r| r.wait || r.status == "applying")
            .cloned()
            .collect::<Vec<_>>();
        for mut request in requests {
            request.wait = false;
            if request.status == "applying" {
                request.status = "pending".into();
                request.resolved_by = None;
                request.reason=Some("Apply interrupted by restart; the original revision will be revalidated before any retry.".into());
            }
            self.save_scope_request(request)?;
        }
        let ended = self
            .journal
            .lock()
            .unwrap()
            .agents
            .values()
            .filter(|a| {
                a.agent_id != "root"
                    && matches!(
                        a.status.as_str(),
                        "done" | "failed" | "roadblocked" | "killed" | "interrupted"
                    )
            })
            .map(|a| (a.agent_id.clone(), a.status.clone()))
            .collect::<Vec<_>>();
        for (id, status) in ended {
            self.scope_owner_ended(&id, &status)?;
        }
        // Backfill owner rows for sessions written before owner journaling, or
        // interrupted between the requester and owner appends. No new decision
        // is inferred for legacy records without resolved_by.
        let envelopes = {
            let mut j = self.journal.lock().unwrap();
            let missing = j
                .scope_requests
                .values()
                .filter(|r| !j.read_only && j.scope_owner_records.get(&r.id) != Some(&r.updated))
                .cloned()
                .collect::<Vec<_>>();
            let mut envelopes = Vec::new();
            for request in missing {
                let owner = request.owner.clone();
                envelopes.push(j.append(&owner, HistoryEvent::ScopeRequest { request })?);
            }
            envelopes
        };
        for envelope in envelopes {
            self.emit(EngineEvent::Committed { envelope });
        }
        Ok(())
    }
    pub(crate) fn notice_parent_scope_write(
        &self,
        writer: &str,
        path: &Path,
    ) -> Result<(), EngineError> {
        let children = {
            let j = self.journal.lock().unwrap();
            j.scopes
                .iter()
                .filter(|(id, s)| {
                    id.as_str() != writer
                        && active(&j, id)
                        && s.exclusive
                        && ancestor(&j, writer, id)
                        && s.paths.iter().any(|p| path.starts_with(p))
                        && !j
                            .subscriptions
                            .get(*id)
                            .is_some_and(|paths| paths.iter().any(|p| path.starts_with(p)))
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        for child in children {
            self.message(writer,&child,format!("Parent {writer} wrote {} inside your active scope. Re-read the file before editing; your write ownership remains active.",path.display()))?;
        }
        Ok(())
    }
}
impl Control {
    pub(crate) fn scope_context_snapshot(&self, agent: &str) -> String {
        let j = self.journal.lock().unwrap();
        let requests=j.scope_requests.values().filter(|r|matches!(r.status.as_str(),"pending"|"applying") && (r.requester==agent || r.owner==agent)).map(|r|json!({"id":r.id,"requester":r.requester,"owner":r.owner,"path":r.path,"status":r.status,"wait":r.wait,"escalation":r.escalation})).collect::<Vec<_>>();
        format!(
            "Authoritative write scope and unresolved requests: {}. Use scope.requests() for full request payloads. Reads and bash are unrestricted by scopes; ordinary permissions still apply.",
            json!({"scope":effective(&j,agent),"requests":requests})
        )
    }
}
impl Control {
    pub(crate) fn begin_dependency_wait(
        &self,
        from: &str,
        to: &str,
    ) -> Result<String, EngineError> {
        let mut j = self.journal.lock().unwrap();
        refuse_cycle(&j, from, to, None)?;
        let id = uuid::Uuid::new_v4().to_string();
        j.wait_edges.insert(id.clone(), (from.into(), to.into()));
        Ok(id)
    }
    pub(crate) fn end_dependency_wait(&self, edge: &str) {
        self.journal.lock().unwrap().wait_edges.remove(edge);
    }
}
impl Control {
    pub(crate) fn scope_turn_serviced(&self, agent: &str) {
        let ids = self
            .journal
            .lock()
            .unwrap()
            .scope_requests
            .values()
            .filter(|r| r.owner == agent && matches!(r.status.as_str(), "pending" | "applying"))
            .map(|r| r.id.clone())
            .collect::<Vec<_>>();
        self.scope_serviced.lock().unwrap().extend(ids);
        self.scheduler.release_owner(agent);
    }
}
