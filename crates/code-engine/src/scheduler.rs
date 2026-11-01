//! FIFO context reservations. Runs retain reservations across tool calls and yield
//! to constrained children and short calls. Cancellation drops every reservation.
use crate::{model::Model, schema::EngineError};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Target(BTreeMap<String, usize>);
impl Target {
    pub fn model(model: &Model) -> Self {
        let mut limits = BTreeMap::new();
        limits.insert(
            format!("provider:{}", model.provider_id),
            model.provider.max_concurrent_contexts.unwrap_or(usize::MAX),
        );
        limits.insert(
            format!("model:{}/{}", model.provider_id, model.model_id),
            model.config.max_concurrent_contexts.unwrap_or(usize::MAX),
        );
        Self(limits)
    }
    fn overlaps(&self, other: &Self) -> bool {
        self.0.iter().any(|(k, limit)| {
            other
                .0
                .get(k)
                .is_some_and(|n| (*n).min(*limit) != usize::MAX)
        })
    }
}
struct Entry {
    target: Target,
    held: bool,
    blockers: BTreeSet<String>,
    display: String,
    parent: Option<String>,
    parked: bool,
    inflight: bool,
}
#[derive(Default)]
struct Queue {
    priority: BTreeMap<String, BTreeSet<String>>,
    entries: BTreeMap<String, Entry>,
    fifo: VecDeque<String>,
}
type Status = Arc<dyn Fn(&str, &str) + Send + Sync>;
#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    changed: Notify,
}
pub(crate) struct Scheduler {
    shared: Arc<Shared>,
    status: Mutex<Option<Status>>,
    namespace: String,
}
impl Default for Scheduler {
    fn default() -> Self {
        static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
        Self {
            shared: SHARED.get_or_init(Arc::default).clone(),
            status: Mutex::default(),
            namespace: uuid::Uuid::new_v4().to_string(),
        }
    }
}
pub(crate) struct Reservation {
    scheduler: Arc<Scheduler>,
    id: String,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.scheduler.finish(&self.id);
    }
}
pub(crate) struct InFlight {
    scheduler: Arc<Scheduler>,
    id: String,
}
impl Drop for InFlight {
    fn drop(&mut self) {
        let mut waiting = None;
        let mut q = self.scheduler.shared.queue.lock().unwrap();
        if let Some(entry) = q.entries.get_mut(&self.id) {
            entry.inflight = false;
            if !entry.blockers.is_empty() {
                entry.held = false;
                waiting = Some(entry.display.clone());
            }
        }
        drop(q);
        if let Some(id) = waiting {
            self.scheduler.notify_status(&id, "waiting");
        }
        self.scheduler.shared.changed.notify_waiters();
    }
}
impl Scheduler {
    fn notify_status(&self, id: &str, status: &str) {
        if let Some(callback) = self.status.lock().unwrap().clone() {
            callback(id, status);
        }
    }
    fn key(&self, id: &str) -> String {
        format!("{}:{id}", self.namespace)
    }
    pub fn set_status(&self, status: Status) {
        *self.status.lock().unwrap() = Some(status);
    }
    pub fn register(
        self: &Arc<Self>,
        id: String,
        display: String,
        target: Target,
        parent: Option<&str>,
    ) -> Reservation {
        let id = self.key(&id);
        let parent = parent.map(|p| self.key(p));
        let mut q = self.shared.queue.lock().unwrap();
        let mut waiting = Vec::new();
        let mut ancestor = parent.clone();
        while let Some(ancestor_id) = ancestor {
            let Some(entry) = q.entries.get_mut(&ancestor_id) else {
                break;
            };
            if entry.target.overlaps(&target) {
                if !entry.inflight {
                    entry.held = false;
                    waiting.push(entry.display.clone());
                }
                entry.blockers.insert(id.clone());
            }
            ancestor = entry.parent.clone();
        }
        q.entries.insert(
            id.clone(),
            Entry {
                target,
                held: false,
                blockers: BTreeSet::new(),
                display,
                parent,
                parked: false,
                inflight: false,
            },
        );
        q.fifo.push_back(id.clone());
        drop(q);
        for id in waiting {
            self.notify_status(&id, "waiting");
        }
        self.shared.changed.notify_waiters();
        Reservation {
            scheduler: self.clone(),
            id,
        }
    }
    /// Temporarily let an owner service a scope request ahead of its descendants.
    /// This yields reservations; the requester is parked only by an explicit wait.
    pub fn prioritize_owner(&self, owner: &str) {
        let owner = self.key(owner);
        let mut q = self.shared.queue.lock().unwrap();
        if q.priority.contains_key(&owner) {
            return;
        }
        let Some(entry) = q.entries.get(&owner) else {
            return;
        };
        let target = entry.target.clone();
        let blockers = entry.blockers.clone();
        q.priority.insert(owner.clone(), blockers.clone());
        if let Some(entry) = q.entries.get_mut(&owner) {
            entry.blockers.clear();
        }
        for (id, entry) in &mut q.entries {
            if id != &owner && blockers.contains(id) && entry.target.overlaps(&target) {
                entry.blockers.insert(owner.clone());
                if !entry.inflight {
                    entry.held = false;
                }
            }
        }
        q.fifo.retain(|id| id != &owner);
        q.fifo.push_front(owner);
        drop(q);
        self.shared.changed.notify_waiters();
    }
    pub fn release_owner(&self, owner: &str) {
        let owner = self.key(owner);
        let mut q = self.shared.queue.lock().unwrap();
        let Some(blockers) = q.priority.remove(&owner) else {
            return;
        };
        let live = blockers
            .into_iter()
            .filter(|id| q.entries.get(id).is_some_and(|e| !e.parked))
            .collect::<BTreeSet<_>>();
        for entry in q.entries.values_mut() {
            entry.blockers.remove(&owner);
        }
        if let Some(entry) = q.entries.get_mut(&owner) {
            entry.blockers.extend(live);
            if !entry.inflight && !entry.blockers.is_empty() {
                entry.held = false;
            }
        }
        drop(q);
        self.shared.changed.notify_waiters();
    }
    pub fn pause(&self, id: &str) {
        let id = self.key(id);
        let mut q = self.shared.queue.lock().unwrap();
        if let Some(e) = q.entries.get_mut(&id) {
            e.held = false;
        }
        q.fifo.retain(|i| i != &id);
        drop(q);
        self.shared.changed.notify_waiters();
    }
    pub fn contains(&self, id: &str) -> bool {
        self.shared
            .queue
            .lock()
            .unwrap()
            .entries
            .contains_key(&self.key(id))
    }
    pub fn retarget(&self, id: &str, target: Target) {
        let id = self.key(id);
        let mut q = self.shared.queue.lock().unwrap();
        if q.entries.get(&id).is_none_or(|e| e.target == target) {
            return;
        }
        q.entries.get_mut(&id).unwrap().target = target.clone();
        q.entries.get_mut(&id).unwrap().held = false;
        q.fifo.retain(|i| i != &id);
        let descendants = q
            .entries
            .iter()
            .filter_map(|(child_id, child)| {
                let mut ancestor = child.parent.as_ref();
                while let Some(parent) = ancestor {
                    if parent == &id {
                        return (!child.parked && child.target.overlaps(&target))
                            .then(|| child_id.clone());
                    }
                    ancestor = q.entries.get(parent).and_then(|e| e.parent.as_ref());
                }
                None
            })
            .collect();
        q.entries.get_mut(&id).unwrap().blockers = descendants;
        for entry in q.entries.values_mut() {
            entry.blockers.remove(&id);
        }
        let mut ancestor = q.entries.get(&id).and_then(|e| e.parent.clone());
        while let Some(parent) = ancestor {
            let Some(entry) = q.entries.get_mut(&parent) else {
                break;
            };
            if entry.target.overlaps(&target) {
                if !entry.inflight {
                    entry.held = false;
                }
                entry.blockers.insert(id.clone());
            }
            ancestor = entry.parent.clone();
        }
        drop(q);
        self.shared.changed.notify_waiters();
    }
    pub fn park(&self, id: &str) {
        self.pause(id);
        let id = self.key(id);
        let mut q = self.shared.queue.lock().unwrap();
        if let Some(e) = q.entries.get_mut(&id) {
            e.parked = true;
        }
        for e in q.entries.values_mut() {
            e.blockers.remove(&id);
        }
        drop(q);
        self.shared.changed.notify_waiters();
    }
    pub fn wake(&self, id: &str) {
        let id = self.key(id);
        let mut q = self.shared.queue.lock().unwrap();
        let Some(entry) = q.entries.get_mut(&id) else {
            return;
        };
        entry.parked = false;
        let target = entry.target.clone();
        let mut ancestor = entry.parent.clone();
        while let Some(parent) = ancestor {
            let Some(entry) = q.entries.get_mut(&parent) else {
                break;
            };
            if entry.target.overlaps(&target) {
                if !entry.inflight {
                    entry.held = false;
                }
                entry.blockers.insert(id.clone());
            }
            ancestor = entry.parent.clone();
        }
        drop(q);
        self.shared.changed.notify_waiters();
    }
    fn finish(&self, id: &str) {
        let mut q = self.shared.queue.lock().unwrap();
        q.priority.remove(id);
        for blockers in q.priority.values_mut() {
            blockers.remove(id);
        }
        q.entries.remove(id);
        q.fifo.retain(|i| i != id);
        for entry in q.entries.values_mut() {
            entry.blockers.remove(id);
        }
        drop(q);
        self.shared.changed.notify_waiters();
    }
    pub async fn acquire(&self, id: &str, cancel: &CancellationToken) -> Result<(), EngineError> {
        let key = self.key(id);
        let id = key.as_str();
        let mut previous = None;
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if cancel.is_cancelled() {
                return Err(EngineError::cancelled());
            }
            let (ready, display, position) = {
                let mut q = self.shared.queue.lock().unwrap();
                let Some(entry) = q.entries.get(id) else {
                    return Err(EngineError::bad("context reservation ended"));
                };
                if entry.held {
                    return Ok(());
                }
                let target = entry.target.clone();
                let display = entry.display.clone();
                let eligible = !entry.parked && entry.blockers.is_empty();
                if eligible && !q.fifo.iter().any(|i| i == id) {
                    q.fifo.push_back(id.into());
                }
                let earlier = q
                    .fifo
                    .iter()
                    .take_while(|i| i.as_str() != id)
                    .filter_map(|i| q.entries.get(i))
                    .filter(|e| !e.parked && e.blockers.is_empty() && e.target.overlaps(&target))
                    .count();
                let capacity = target.0.iter().all(|(key, limit)| {
                    q.entries
                        .values()
                        .filter(|e| e.held && e.target.0.contains_key(key))
                        .count()
                        < *limit
                });
                let ready = eligible && earlier == 0 && capacity;
                if ready {
                    q.entries.get_mut(id).unwrap().held = true;
                    q.fifo.retain(|i| i != id);
                }
                (ready, display, earlier + 1)
            };
            let status = if ready {
                "running".into()
            } else {
                format!("queued:{position}")
            };
            if previous.as_ref() != Some(&status) {
                self.notify_status(&display, &status);
                previous = Some(status);
            }
            if ready {
                self.shared.changed.notify_waiters();
                return Ok(());
            }
            tokio::select! { _ = cancel.cancelled() => return Err(EngineError::cancelled()), _ = &mut changed => {} }
        }
    }
    pub async fn enter(
        self: &Arc<Self>,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<InFlight, EngineError> {
        loop {
            self.acquire(id, cancel).await?;
            let key = self.key(id);
            let mut q = self.shared.queue.lock().unwrap();
            let entry = q
                .entries
                .get_mut(&key)
                .ok_or_else(|| EngineError::bad("context reservation ended"))?;
            if entry.held && entry.blockers.is_empty() && !entry.parked {
                entry.inflight = true;
                return Ok(InFlight {
                    scheduler: self.clone(),
                    id: key,
                });
            }
        }
    }
    pub async fn short(
        self: &Arc<Self>,
        owner: &str,
        model: &Model,
        cancel: &CancellationToken,
    ) -> Result<Reservation, EngineError> {
        self.short_named(owner, owner, model, cancel).await
    }
    pub async fn short_named(
        self: &Arc<Self>,
        owner: &str,
        display: &str,
        model: &Model,
        cancel: &CancellationToken,
    ) -> Result<Reservation, EngineError> {
        let id = format!("short-{}", uuid::Uuid::new_v4());
        let reservation = self.register(
            id.clone(),
            display.into(),
            Target::model(model),
            Some(owner),
        );
        self.acquire(&id, cancel).await?;
        Ok(reservation)
    }
}
