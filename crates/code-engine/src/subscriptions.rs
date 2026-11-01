//! Bounded filesystem subscriptions. Native writes carry exact authorship; disk events are debounced.
use crate::{Control, Session, schema::*, sessions};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionChange {
    pub path: PathBuf,
    pub source: Option<String>,
    pub text: String,
}
#[derive(Clone, Default)]
pub(crate) struct Pending {
    pub changes: BTreeMap<PathBuf, SubscriptionChange>,
    pub overflow: bool,
}
#[derive(Default)]
pub(crate) struct WatchState {
    staging: BTreeSet<PathBuf>,
    watcher: Option<notify::RecommendedWatcher>,
    watched: BTreeSet<PathBuf>,
    seen: BTreeMap<PathBuf, Option<String>>,
    memory: crate::file_notices::ReadMemory,
}
const PATHS: usize = 256;
const BATCH: usize = 64;
fn signature(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() <= 256 * 1024 {
        std::fs::read(path).ok().map(|b| crate::context::digest(&b))
    } else {
        Some(format!("large:{}:{:?}", meta.len(), meta.modified().ok()))
    }
}
fn remember(state: &mut WatchState, path: &Path) {
    if state.seen.len() >= 4096 && !state.seen.contains_key(path) {
        state.seen.clear();
    }
    state.seen.insert(path.into(), signature(path));
    if std::fs::metadata(path).is_ok_and(|m| m.len() <= 256 * 1024)
        && let Ok(bytes) = std::fs::read(path)
    {
        state.memory.remember(path, &bytes);
    }
}
fn directories(j: &sessions::Journal) -> BTreeSet<PathBuf> {
    j.subscriptions.values().flatten().cloned().collect()
}
impl Control {
    pub(crate) fn set_subscriptions(
        &self,
        agent: &str,
        values: &[PathBuf],
    ) -> Result<Vec<PathBuf>, EngineError> {
        if values.len() > 32 {
            return Err(EngineError::bad(
                "subscriptions are capped at 32 directories",
            ));
        }
        let cwd = {
            let j = self.journal.lock().unwrap();
            if !j.agents.contains_key(agent) {
                return Err(EngineError::bad("agent not found"));
            }
            j.executions
                .get(agent)
                .map(|e| e.cwd.clone())
                .unwrap_or_else(|| j.header.cwd.clone())
        };
        let resolver = crate::paths::Resolver::new(cwd);
        let mut paths = BTreeSet::new();
        for raw in values {
            let p = resolver
                .resolve(&raw.to_string_lossy())
                .map_err(|e| EngineError::bad(e.to_string()))?;
            if p.exists() && !p.is_dir() {
                return Err(EngineError::bad("subscriptions require directories"));
            }
            paths.insert(p);
        }
        let paths = paths.into_iter().collect::<Vec<_>>();
        {
            let j = self.journal.lock().unwrap();
            let roots = j
                .subscriptions
                .iter()
                .filter(|(id, _)| id.as_str() != agent)
                .flat_map(|(_, p)| p.iter())
                .chain(paths.iter())
                .collect::<BTreeSet<_>>();
            if roots.len() > 128 {
                return Err(EngineError::bad(
                    "subscriptions are capped at 128 distinct directories across the session",
                ));
            }
        }

        self.commit(
            agent,
            HistoryEvent::Subscriptions {
                paths: paths.clone(),
            },
        )?;
        self.refresh_subscription_watch()?;
        self.changed.notify_waiters();
        Ok(paths)
    }
    fn subscription_change(&self, change: SubscriptionChange) -> Result<(), EngineError> {
        let recipients = {
            let j = self.journal.lock().unwrap();
            j.subscriptions
                .iter()
                .filter(|(_, paths)| paths.iter().any(|p| change.path.starts_with(p)))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        for agent in recipients {
            self.commit(
                &agent,
                HistoryEvent::SubscriptionChanged {
                    change: change.clone(),
                },
            )?;
        }
        Ok(())
    }
    pub(crate) fn subscription_stage(&self, path: &Path) {
        let mut state = self.subscription_watch.lock().unwrap();
        if state.staging.len() >= 1024 {
            state.staging.clear();
        }
        state.staging.insert(path.into());
    }
    pub(crate) fn subscription_write_bytes(
        &self,
        agent: &str,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), EngineError> {
        // Register our committed revision before the watcher can consume its
        // echo. A subsequent editor replacement still belongs to the editor.
        let mut state = self.subscription_watch.lock().unwrap();
        let text = state.memory.notice(path);
        if state.seen.len() >= 4096 && !state.seen.contains_key(path) {
            state.seen.clear();
        }
        let written = if bytes.len() <= 256 * 1024 {
            Some(crate::context::digest(bytes))
        } else {
            signature(path)
        };
        state.seen.insert(path.into(), written.clone());
        state.memory.remember(path, bytes);
        self.subscription_change(SubscriptionChange {
            path: path.into(),
            source: Some(agent.into()),
            text: if text.is_empty() {
                format!("{} changed; re-read it.", path.display())
            } else {
                text
            },
        })?;
        if signature(path) != written {
            let text = state.memory.notice(path);
            remember(&mut state, path);
            self.subscription_change(SubscriptionChange {
                path: path.into(),
                source: None,
                text,
            })?;
        }
        Ok(())
    }
    pub(crate) fn subscription_write(&self, agent: &str, path: &Path) -> Result<(), EngineError> {
        let mut state = self.subscription_watch.lock().unwrap();
        let text = state.memory.notice(path);
        remember(&mut state, path);
        drop(state);
        self.subscription_change(SubscriptionChange {
            path: path.into(),
            source: Some(agent.into()),
            text: if text.is_empty() {
                format!("{} changed; re-read it.", path.display())
            } else {
                text
            },
        })
    }
    pub(crate) fn deliver_subscriptions(&self, agent: &str) -> Result<(), EngineError> {
        let (envelope, changes, overflow) = {
            let mut j = self.journal.lock().unwrap();
            let pending = j
                .subscription_pending
                .get(agent)
                .cloned()
                .unwrap_or_default();
            if pending.changes.is_empty() && !pending.overflow {
                return Ok(());
            }
            let changes = pending.changes.into_values().collect::<Vec<_>>();
            let mut text =
                String::from("Subscribed paths changed (other agents or external editor):\n");
            let mut overflow = pending.overflow;
            for change in &changes {
                if text.len() + change.text.len() > 8192 {
                    overflow = true;
                    break;
                }
                text.push_str(&change.text);
                text.push('\n');
            }
            if overflow {
                text.push_str("Additional changes were coalesced or exceed this bounded notice; re-read subscribed paths.\n");
            }
            let envelope = j.append(
                agent,
                HistoryEvent::SubscriptionNotice {
                    changes: changes.clone(),
                    overflow,
                    text,
                },
            )?;
            (envelope, changes, overflow)
        };
        self.emit(EngineEvent::Committed { envelope });
        self.emit(EngineEvent::SubscriptionNotice {
            agent_id: agent.into(),
            changes,
            overflow,
        });
        Ok(())
    }
    fn refresh_subscription_watch(&self) -> Result<(), EngineError> {
        let roots = {
            let j = self.journal.lock().unwrap();
            directories(&j)
        };
        let actual = roots
            .iter()
            .filter_map(|p| {
                let mut p = p.clone();
                while !p.is_dir() {
                    if !p.pop() {
                        return None;
                    }
                }
                Some(p)
            })
            .collect::<BTreeSet<_>>();
        let mut state = self.subscription_watch.lock().unwrap();
        let previous = state.watched.clone();
        for path in previous.difference(&actual) {
            if let Some(w) = state.watcher.as_mut() {
                let _ = w.unwatch(path);
            }
        }
        for path in actual.difference(&previous) {
            state
                .watcher
                .as_mut()
                .ok_or_else(|| EngineError::bad("filesystem subscription watcher is unavailable"))?
                .watch(path, RecursiveMode::Recursive)
                .map_err(|e| EngineError::bad(e.to_string()))?;
            let started = std::time::Instant::now();
            for entry in ignore::WalkBuilder::new(path)
                .hidden(false)
                .build()
                .take(1024)
                .take_while(|_| started.elapsed() < Duration::from_millis(150))
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                .take(256)
            {
                remember(&mut state, entry.path());
            }
        }
        state.watched = actual;
        Ok(())
    }
    pub(crate) fn start_subscription_watch(self: &Arc<Self>) -> Result<(), EngineError> {
        let weak = Arc::downgrade(self);
        let cancel = self.health_cancel.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(PATHS);
        let overflow = Arc::new(AtomicBool::new(false));
        let flag = overflow.clone();
        let watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                match event {
                    Ok(event)
                        if matches!(
                            event.kind,
                            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                        ) =>
                    {
                        for path in event.paths {
                            if tx.try_send(path).is_err() {
                                flag.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(_) => {
                        flag.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
            })
            .map_err(|e| EngineError::bad(e.to_string()))?;
        self.subscription_watch.lock().unwrap().watcher = Some(watcher);
        self.refresh_subscription_watch()?;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(300));
            loop {
                tokio::select! {_=cancel.cancelled()=>break,_=tick.tick()=>{}}
                let Some(control) = weak.upgrade() else {
                    break;
                };
                let roots = {
                    let j = control.journal.lock().unwrap();
                    directories(&j)
                };
                let mut paths = BTreeSet::new();
                while let Ok(path) = rx.try_recv() {
                    if paths.len() < PATHS {
                        paths.insert(path);
                    } else {
                        overflow.store(true, Ordering::Relaxed);
                    }
                }
                for path in paths {
                    if !roots.iter().any(|p| path.starts_with(p)) || path.is_dir() {
                        continue;
                    }
                    let mut state = control.subscription_watch.lock().unwrap();
                    if state.staging.contains(&path) {
                        continue;
                    }
                    let hash = signature(&path);
                    if hash.is_none() && !state.seen.contains_key(&path) {
                        continue;
                    }
                    if state.seen.get(&path).is_some_and(|old| *old == hash) {
                        continue;
                    }
                    let text = state.memory.notice(&path);
                    remember(&mut state, &path);
                    drop(state);
                    if control
                        .subscription_change(SubscriptionChange {
                            path,
                            source: None,
                            text,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                if overflow.swap(false, Ordering::Relaxed) {
                    let agents = control
                        .journal
                        .lock()
                        .unwrap()
                        .subscriptions
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>();
                    for agent in agents {
                        let _ = control.commit(&agent, HistoryEvent::SubscriptionOverflow);
                    }
                }
            }
        });
        Ok(())
    }
}
impl Session {
    pub fn subscriptions(&self) -> BTreeMap<String, Vec<PathBuf>> {
        self.control.journal.lock().unwrap().subscriptions.clone()
    }
    pub fn set_subscriptions(
        &self,
        agent_id: &str,
        paths: Vec<PathBuf>,
    ) -> Result<Vec<PathBuf>, EngineError> {
        self.control.set_subscriptions(agent_id, &paths)
    }
}
pub(crate) fn apply_pending(pending: &mut Pending, change: SubscriptionChange) {
    if pending.changes.len() >= BATCH && !pending.changes.contains_key(&change.path) {
        pending.overflow = true;
    } else {
        pending.changes.insert(change.path.clone(), change);
    }
}
pub(crate) async fn task_subscriptions(
    state: &crate::State,
    id: &str,
    paths: &[PathBuf],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), EngineError> {
    let cwd = state
        .control
        .journal
        .lock()
        .unwrap()
        .executions
        .get(id)
        .map(|e| e.cwd.clone())
        .unwrap_or_else(|| state.cwd.clone());
    let resolver = crate::paths::Resolver::new(cwd.clone());
    for raw in paths {
        let path = resolver
            .resolve(&raw.to_string_lossy())
            .map_err(|e| EngineError::bad(e.to_string()))?;
        if !path.starts_with(&cwd) {
            let call = ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "read".into(),
                arguments: serde_json::json!({"path":path}),
                complete: true,
            };
            let mut operation = state.metadata_operation(call, Some(path));
            operation.cwd = cwd.clone();
            state
                .gate
                .authorize(&operation, cancel)
                .await
                .map_err(|e| EngineError::bad(e.to_string()))?;
        }
    }
    state.control.set_subscriptions(id, paths)?;
    Ok(())
}
pub(crate) fn file_signature(path: &Path) -> Option<String> {
    signature(path)
}
pub(crate) fn shell_write_paths(command: &str) -> Vec<String> {
    static REDIRECT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = REDIRECT.get_or_init(|| {
        regex::Regex::new(r#"(?:^|[^<])>{1,2}\s*(?:"([^"]+)"|'([^']+)'|([^\s;|&<>]+))"#).unwrap()
    });
    re.captures_iter(command)
        .filter_map(|c| (1..=3).find_map(|i| c.get(i).map(|m| m.as_str().to_owned())))
        .filter(|p| !p.contains(['$', '`']))
        .take(64)
        .collect()
}
