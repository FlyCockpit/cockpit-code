//! Project reviewer guidance is user-trusted content, pinned outside the repository.
use crate::{
    Control, Session,
    schema::{EngineError, EngineEvent},
    tools::PreparedOperation,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewerGuidanceRequest {
    #[serde(default)]
    pub library: bool,
    pub id: String,
    pub path: PathBuf,
    pub sha256: String,
    pub content: String,
    #[serde(default)]
    pub tampered: bool,
    #[serde(default)]
    pub deleted: bool,
}
#[derive(Default)]
pub(crate) struct Trust {
    pending: BTreeMap<String, ReviewerGuidanceRequest>,
    decisions: BTreeMap<PathBuf, (String, bool)>,
    watched: BTreeMap<PathBuf, Pin>,
    approved: BTreeMap<String, ApprovedWrite>,
}
#[derive(Clone)]
struct Pin {
    content: Option<Vec<u8>>,
    sha256: Option<String>,
    parent: PathBuf,
}
struct ApprovedWrite {
    paths: Vec<PathBuf>,
    job_id: Option<String>,
}
impl Session {
    pub fn pending_reviewer_guidance(&self) -> Vec<ReviewerGuidanceRequest> {
        self.control
            .guidance
            .lock()
            .unwrap()
            .pending
            .values()
            .cloned()
            .collect()
    }
    pub fn resolve_reviewer_guidance(&self, id: &str, trust: bool) -> Result<(), EngineError> {
        self.control.check_owner()?;
        let mut state = self.control.guidance.lock().unwrap();
        let request = state
            .pending
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::bad("reviewer guidance request not found"))?;
        if trust {
            let bytes = if request.deleted {
                None
            } else {
                Some(request.content.as_bytes().to_vec())
            };
            if request.tampered {
                let pin = state
                    .watched
                    .get(&request.path)
                    .ok_or_else(|| EngineError::bad("reviewer guidance baseline not found"))?;
                if read_content(&request.path)? != pin.content {
                    return Err(EngineError::bad(
                        "reviewer guidance changed after restoration; decline this request and review the new content",
                    ));
                }
                self.control
                    .journal
                    .lock()
                    .unwrap()
                    .with_owner(Box::new(|| {
                        validate_parent(&request.path, &pin.parent)?;
                        replace_content(&request.path, bytes.as_deref())
                            .map_err(anyhow::Error::from)
                    }))
                    .map_err(|e| EngineError::bad(e.to_string()))?;
            } else if read_content(&request.path)?
                .as_deref()
                .map(crate::context::digest)
                .as_deref()
                != Some(&request.sha256)
            {
                return Err(EngineError::bad(
                    "reviewer guidance changed; decline this request and review the new content",
                ));
            }
            pin_content(&self.control.data_dir, &request.path, bytes.as_deref())?;
            if !request.library {
                state.watched.insert(
                    request.path.clone(),
                    Pin {
                        sha256: bytes.as_deref().map(crate::context::digest),
                        content: bytes,
                        parent: resolved_parent(&request.path)?,
                    },
                );
            }
        }
        state.pending.remove(id);
        self.control.save_guidance_pending(&state.pending)?;
        state
            .decisions
            .insert(request.path, (request.sha256, trust));
        drop(state);
        self.control.changed.notify_waiters();
        Ok(())
    }
}
fn resolved_parent(path: &Path) -> Result<PathBuf, EngineError> {
    let parent = path
        .parent()
        .ok_or_else(|| EngineError::bad("guidance path has no parent"))?;
    crate::paths::Resolver::new(parent.into())
        .resolve(&parent.to_string_lossy())
        .map_err(|e| EngineError::bad(e.to_string()))
}
fn validate_parent(path: &Path, expected: &Path) -> anyhow::Result<()> {
    if resolved_parent(path).map_err(|e| anyhow::anyhow!(e.message))? != expected {
        anyhow::bail!(
            "guidance directory was redirected; refusing to restore through a changed symlink"
        );
    }
    Ok(())
}
fn read_content(path: &Path) -> Result<Option<Vec<u8>>, EngineError> {
    use std::io::Read;
    match std::fs::File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(128 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| {
                    EngineError::bad(format!(
                        "cannot read reviewer guidance {}: {e}",
                        path.display()
                    ))
                })?;
            if bytes.len() > 128 * 1024 {
                return Err(EngineError::bad("reviewer guidance exceeds 128 KiB"));
            }
            Ok(Some(bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(EngineError::bad(format!(
            "cannot read reviewer guidance {}: {e}",
            path.display()
        ))),
    }
}
fn replace_content(path: &Path, bytes: Option<&[u8]>) -> Result<(), EngineError> {
    let bad = |e: std::io::Error| EngineError::bad(e.to_string());
    if let Some(bytes) = bytes {
        let parent = path
            .parent()
            .ok_or_else(|| EngineError::bad("guidance path has no parent"))?;
        std::fs::create_dir_all(parent).map_err(bad)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(bad)?;
        use std::io::Write;
        temp.write_all(bytes)
            .and_then(|_| temp.as_file().sync_all())
            .map_err(bad)?;
        temp.persist(path).map_err(|e| bad(e.error))?;
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(bad(e)),
        }
    }
    Ok(())
}
fn pin_content(data: &Path, path: &Path, bytes: Option<&[u8]>) -> Result<(), EngineError> {
    std::fs::create_dir_all(data).map_err(|e| EngineError::bad(e.to_string()))?;
    // Sessions share DATA; serialize the read/modify/replace across processes.
    use std::os::fd::AsRawFd;
    let guard = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data.join(".reviewer-guidance.guard"))
        .map_err(|e| EngineError::bad(e.to_string()))?;
    if unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(EngineError::bad(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let pins_path = data.join("reviewer-guidance-trust.json");
    let mut pins = read_pins(&pins_path);
    if let Some(bytes) = bytes {
        let hash = crate::context::digest(bytes);
        replace_content(
            &data.join("reviewer-guidance-content").join(&hash),
            Some(bytes),
        )?;
        pins.insert(path.into(), hash);
    } else {
        pins.remove(path);
    }
    crate::sessions::write_json(&pins_path, &pins)
}
fn read_pins(path: &Path) -> BTreeMap<PathBuf, String> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
impl Control {
    fn save_guidance_pending(
        &self,
        pending: &BTreeMap<String, ReviewerGuidanceRequest>,
    ) -> Result<(), EngineError> {
        let mut journal = self.journal.lock().unwrap();
        journal.ensure_persisted()?;
        let path = journal.dir.join("reviewer-guidance-pending.json");
        journal
            .with_owner(Box::new(|| {
                crate::sessions::write_json(&path, pending).map_err(anyhow::Error::from)
            }))
            .map_err(|e| EngineError::bad(e.to_string()))
    }
    pub(crate) fn restore_guidance_requests(&self) -> Result<(), EngineError> {
        let path = self
            .journal
            .lock()
            .unwrap()
            .dir
            .join("reviewer-guidance-pending.json");
        if !path.exists() {
            return Ok(());
        }
        let pending: BTreeMap<String, ReviewerGuidanceRequest> = crate::sessions::read_json(&path)?;
        for request in pending.values() {
            if request.library {
                continue;
            }
            if let Some(cwd) = request.path.parent().and_then(Path::parent) {
                self.watch_reviewer_guidance(cwd)?;
            }
        }
        self.guidance.lock().unwrap().pending = pending;
        Ok(())
    }
    /// Register before execution so legacy hash-only pins can capture matching bytes.
    pub(crate) fn watch_reviewer_guidance(&self, cwd: &Path) -> Result<(), EngineError> {
        let path = cwd.join(".cockpit-code/reviewer-guidance.md");
        let mut state = self.guidance.lock().unwrap();
        if state.watched.contains_key(&path) {
            return Ok(());
        }
        let pins = read_pins(&self.data_dir.join("reviewer-guidance-trust.json"));
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let content = if let Some(hash) = pins.get(&path).or_else(|| pins.get(&canonical)) {
            let blob = self.data_dir.join("reviewer-guidance-content").join(hash);
            let bytes = match read_content(&blob)? {
                Some(bytes) => Some(bytes),
                None => read_content(&path)?,
            };
            let bytes = bytes.filter(|b| crate::context::digest(b) == *hash);
            if let Some(bytes) = &bytes {
                pin_content(&self.data_dir, &path, Some(bytes))?;
            }
            bytes
        } else {
            // Trust controls prompt use, not whether an existing file may be read.
            // Capture the initial bytes so a read-only call is not called tampering.
            read_content(&path)?
        };
        state.watched.insert(
            path,
            Pin {
                sha256: content.as_deref().map(crate::context::digest),
                content,
                parent: resolved_parent(&cwd.join(".cockpit-code/reviewer-guidance.md"))?,
            },
        );
        Ok(())
    }
    pub(crate) fn approve_guidance_write(&self, op: &PreparedOperation) -> Result<(), EngineError> {
        self.watch_reviewer_guidance(&op.cwd)?;
        if let Some(target) = &op.target
            && target.ends_with(".cockpit-code/reviewer-guidance.md")
            && let Some(cwd) = target.parent().and_then(Path::parent)
        {
            self.watch_reviewer_guidance(cwd)?;
        }
        let mut state = self.guidance.lock().unwrap();
        let paths = state
            .watched
            .keys()
            .filter(|path| {
                op.target.as_ref().is_some_and(|target| {
                    *path == target || std::fs::canonicalize(path).is_ok_and(|p| p == *target)
                })
            })
            .cloned()
            .collect();
        state.approved.insert(
            op.call.id.clone(),
            ApprovedWrite {
                paths,
                job_id: op.job_id.clone(),
            },
        );
        Ok(())
    }
    pub(crate) fn guidance_job(&self, call_id: &str, job_id: &str) {
        if let Some(write) = self.guidance.lock().unwrap().approved.get_mut(call_id) {
            write.job_id = Some(job_id.into());
        }
    }
    /// One metadata check and at most one content hash per registered cwd.
    /// Never waits on a trust decision: restore first, then show the candidate.
    pub(crate) fn check_reviewer_guidance(
        &self,
        call_id: Option<&str>,
        job_id: Option<&str>,
        completed: bool,
    ) -> String {
        if let Err(e) = self.check_owner() {
            return format!("\n[reviewer-guidance check failed: {}]", e.message);
        }
        let mut state = self.guidance.lock().unwrap();
        // Approval remains scoped to its path until that operation/job completes.
        // A concurrent read/check must not undo an in-flight approved replacement.
        let approved = state
            .approved
            .values()
            .flat_map(|write| write.paths.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>();
        let mut flags = Vec::new();
        let mut requests = Vec::new();
        for (path, mut pin) in state.watched.clone() {
            // A missing file is a content change too. Hash even if mtime/length match.
            let bytes = match std::fs::symlink_metadata(&path) {
                Ok(_) => match read_content(&path) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        let restored = self.journal.lock().unwrap().with_owner(Box::new(|| {
                            validate_parent(&path, &pin.parent)?;
                            replace_content(&path, pin.content.as_deref())
                                .map_err(anyhow::Error::from)
                        }));
                        flags.push(format!("REVIEWER-GUIDANCE TAMPER: {}; restoration: {}. Cannot offer unreadable content for trust.",
                            e.message, restored.map_or_else(|e|format!("FAILED: {e}"), |_|"restored approved state".into())));
                        continue;
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    flags.push(format!(
                        "reviewer-guidance stat failed: {}: {e}",
                        path.display()
                    ));
                    continue;
                }
            };
            let hash = bytes.as_deref().map(crate::context::digest);
            if hash == pin.sha256 {
                continue;
            }
            // Refresh shared approvals only on a mismatch; unchanged calls do no DATA I/O.
            let pins = read_pins(&self.data_dir.join("reviewer-guidance-trust.json"));
            if pins.get(&path) != pin.sha256.as_ref() {
                let shared = pins.get(&path).cloned();
                let content = if shared == hash {
                    bytes.clone()
                } else if let Some(hash) = &shared {
                    read_content(&self.data_dir.join("reviewer-guidance-content").join(hash))
                        .ok()
                        .flatten()
                        .filter(|b| crate::context::digest(b) == *hash)
                } else {
                    None
                };
                pin.sha256 = content.as_deref().map(crate::context::digest);
                pin.content = content;
                state.watched.insert(path.clone(), pin.clone());
                if hash == pin.sha256 {
                    continue;
                }
            }
            if approved.contains(&path) {
                if let Err(e) = pin_content(&self.data_dir, &path, bytes.as_deref()) {
                    flags.push(format!("approved reviewer-guidance pin failed: {}", e.message));
                } else {
                    state.watched.insert(
                        path,
                        Pin {
                            sha256: hash,
                            content: bytes,
                            parent: pin.parent,
                        },
                    );
                }
                continue;
            }
            let restored = self.journal.lock().unwrap().with_owner(Box::new(|| {
                validate_parent(&path, &pin.parent)?;
                replace_content(&path, pin.content.as_deref()).map_err(anyhow::Error::from)
            }));
            let restoration = match restored {
                Ok(()) => {
                    if pin.content.is_some() {
                        "restored approved content".into()
                    } else {
                        "removed unapproved file (no approved content)".into()
                    }
                }
                Err(e) => format!("RESTORATION FAILED: {e}"),
            };
            flags.push(format!("REVIEWER-GUIDANCE TAMPER: {} changed without a user-approved write; {restoration}. The proposed content requires user trust in every mode.",path.display()));
            let content = match bytes.as_deref().map(std::str::from_utf8).transpose() {
                Ok(content) if content.is_none_or(|s| s.len() <= 128 * 1024) => {
                    content.unwrap_or("").to_owned()
                }
                _ => {
                    flags.push("reviewer-guidance candidate is not UTF-8 guidance within 128 KiB; it cannot be trusted.".into());
                    continue;
                }
            };
            let sha256 = hash.unwrap_or_else(|| crate::context::digest(b""));
            let deleted = bytes.is_none();
            if state
                .pending
                .values()
                .any(|r| r.path == path && r.sha256 == sha256 && r.deleted == deleted)
            {
                continue;
            }
            let request = ReviewerGuidanceRequest {
                library: false,
                id: uuid::Uuid::new_v4().to_string(),
                path,
                sha256,
                content,
                tampered: true,
                deleted,
            };
            state.pending.insert(request.id.clone(), request.clone());
            requests.push(request);
        }
        if completed {
            state.approved.retain(|id, write| {
                !(call_id == Some(id.as_str())
                    || job_id.is_some_and(|job| write.job_id.as_deref() == Some(job)))
            });
        }
        if !requests.is_empty()
            && let Err(e) = self.save_guidance_pending(&state.pending)
        {
            flags.push(format!(
                "reviewer-guidance trust request persistence failed: {}",
                e.message
            ));
        }
        drop(state);
        for request in requests {
            self.emit(EngineEvent::ReviewerGuidanceTrustRequest { request });
        }
        if flags.is_empty() {
            String::new()
        } else {
            format!("\n[{}]", flags.join("\n"))
        }
    }

    pub(crate) async fn project_reviewer_guidance(
        &self,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, EngineError> {
        self.project_guidance_file(
            &cwd.join(".cockpit-code/reviewer-guidance.md"),
            false,
            cancel,
        )
        .await
    }
    pub(crate) async fn project_library_guidance(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, EngineError> {
        self.project_guidance_file(path, true, cancel).await
    }
    async fn project_guidance_file(
        &self,
        path: &Path,
        library: bool,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, EngineError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let path = path.to_path_buf();
            if !library && let Some(cwd) = path.parent().and_then(Path::parent) {
                self.watch_reviewer_guidance(cwd)?;
            }
            if !path.exists() {
                return Ok(None);
            }
            let Some(bytes) = read_content(&path)? else {
                return Ok(None);
            };
            let content = String::from_utf8(bytes)
                .map_err(|e| EngineError::bad(format!("cannot read reviewer guidance: {e}")))?;
            let hash = crate::context::digest(content.as_bytes());
            if read_pins(&self.data_dir.join("reviewer-guidance-trust.json")).get(&path)
                == Some(&hash)
            {
                return Ok(Some(content));
            }
            let waiting_path = path.clone();
            let request = {
                let mut state = self.guidance.lock().unwrap();
                if let Some((seen, allowed)) = state.decisions.get(&path)
                    && seen == &hash
                {
                    return Ok(allowed.then_some(content));
                }
                if state
                    .pending
                    .values()
                    .any(|r| r.path == path && r.sha256 == hash)
                {
                    None
                } else {
                    let request = ReviewerGuidanceRequest {
                        library,
                        id: uuid::Uuid::new_v4().to_string(),
                        path,
                        sha256: hash,
                        content,
                        tampered: false,
                        deleted: false,
                    };
                    state.pending.insert(request.id.clone(), request.clone());
                    self.save_guidance_pending(&state.pending)?;
                    Some(request)
                }
            };
            if let Some(request) = request {
                self.emit(EngineEvent::ReviewerGuidanceTrustRequest { request });
            }
            // A library is advisory: keep trust pending without holding the
            // author's write result. A later load observes any new trust pin.
            if library {
                return Ok(None);
            }
            tokio::select! {
                _=cancel.cancelled()=>{
                    let mut state = self.guidance.lock().unwrap();
                    state.pending.retain(|_,r| r.path != waiting_path || r.tampered);
                    let _ = self.save_guidance_pending(&state.pending);
                    self.changed.notify_waiters();
                    return Err(EngineError::cancelled());
                },
                _=&mut changed=>{}
            }
        }
    }
}
/// Explicit file mutations need approval. Shells/scripts use the post-call hash check.
pub(crate) fn protected_write(op: &PreparedOperation) -> bool {
    let project = op.cwd.join(".cockpit-code/reviewer-guidance.md");
    if matches!(op.call.name.as_str(), "write" | "edit" | "generate")
        && op.target.as_ref().is_some_and(|p| {
            p.ends_with(".cockpit-code/reviewer-guidance.md")
                || std::fs::canonicalize(&project).is_ok_and(|t| t == *p)
        })
    {
        return true;
    }
    false
}
