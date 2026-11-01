//! Frontends start from static/cached models; only the requested provider is awaited.
use crate::{
    Control, Session, State,
    schema::{EngineError, EngineEvent},
};
use code_config::{ConfigStore, Snapshot};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub(crate) fn pending_model(snapshot: &Snapshot, id: &str) -> bool {
    let Some((provider, model)) = id.split_once('/') else {
        return false;
    };
    snapshot.providers.get(provider).is_some_and(|p| {
        p.enabled
            && !p.models.contains_key(model)
            && p.command
                .as_ref()
                .and_then(|c| c.discover.as_ref())
                .is_some()
    })
}

impl Session {
    /// The most recently applied configuration, without I/O.
    pub fn configuration(&self) -> Snapshot {
        self.config.lock().unwrap().clone()
    }
    pub fn discovery_running(&self) -> bool {
        !self.control.discovery_pending.lock().unwrap().is_empty()
    }
    /// Coalesced, nonblocking refresh; each provider runs independently.
    pub fn refresh_discovery(&self) {
        let ids: Vec<_> = self
            .configuration()
            .providers
            .into_iter()
            .filter(|(_, p)| {
                p.enabled
                    && p.command
                        .as_ref()
                        .and_then(|c| c.discover.as_ref())
                        .is_some()
            })
            .map(|(id, _)| id)
            .collect();
        for id in ids {
            self.control.discover_provider(&id);
        }
    }
    /// Wait at most 32 seconds for this model's provider; does not hold state/UI locks.
    pub async fn await_model(&self, id: &str) -> Result<(), EngineError> {
        self.control
            .await_model(id, &CancellationToken::new())
            .await
    }
    /// Validate discovery before committing an explicit model selection.
    pub async fn set_model_ready(&self, id: impl Into<String>) -> Result<(), EngineError> {
        let id = id.into();
        self.await_model(&id).await?;
        self.set_model(id)
    }
}
impl Control {
    fn discover_provider(self: &Arc<Self>, id: &str) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut work = self.title_work.lock().unwrap();
        if work.closing || !self.discovery_pending.lock().unwrap().insert(id.into()) {
            return;
        }
        let id = id.to_owned();
        let control = self.clone();
        let dirs = self.directories.clone();
        work.tasks.retain(|t| !t.is_finished());
        work.tasks.push(runtime.spawn(async move {
            let provider_id = id.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut store = ConfigStore::load(dirs)?;
                store.refresh_discovery_provider(&provider_id)?;
                Ok::<_, code_config::ConfigError>(store)
            })
            .await;
            let errors = match result {
                Ok(Ok(store)) => {
                    let next = store.snapshot();
                    let mut config = control.config.lock().unwrap();
                    if let Some(provider) = next.providers.get(&id) {
                        config.providers.insert(id.clone(), provider.clone());
                    }
                    config.discovery_errors.remove(&id);
                    if let Some(error) = next.discovery_errors.get(&id) {
                        config.discovery_errors.insert(id.clone(), error.clone());
                    }
                    control.usage.update_config(&config);
                    next.errors.clone()
                }
                Ok(Err(error)) => vec![error.to_string()],
                Err(_) => vec![format!("{id}: discovery background worker failed")],
            };
            control.discovery_pending.lock().unwrap().remove(&id);
            control.emit(EngineEvent::DiscoveryUpdated { errors });
            control.changed.notify_waiters();
        }));
    }
    pub(crate) async fn await_model(
        self: &Arc<Self>,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), EngineError> {
        let snapshot = self.config.lock().unwrap().clone();
        if !pending_model(&snapshot, id) {
            return Ok(());
        }
        let provider = id.split_once('/').unwrap().0;
        self.discover_provider(provider);
        let wait = async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !self.discovery_pending.lock().unwrap().contains(provider) {
                    break;
                }
                changed.await;
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => return Err(EngineError::cancelled()),
            result = tokio::time::timeout(Duration::from_secs(32), wait) => {
                if result.is_err() {
                    return Err(EngineError::bad(format!("model discovery for provider {provider} timed out after 32 seconds while waiting for {id}; retry discovery or configure the model explicitly")));
                }
            }
        }
        let snapshot = self.config.lock().unwrap();
        if pending_model(&snapshot, id) {
            let reason = snapshot
                .discovery_errors
                .get(provider)
                .map(|e| e.message.as_str())
                .unwrap_or("provider did not advertise the requested model");
            return Err(EngineError::bad(format!(
                "model {id} unavailable after discovery for provider {provider}: {reason}"
            )));
        }
        Ok(())
    }
}
impl State {
    pub(crate) async fn await_model(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<(), EngineError> {
        let settings = self.model_settings();
        let id = settings
            .exclusive
            .as_ref()
            .map(|e| &e.model)
            .or(settings.model.as_ref())
            .or(self.store.snapshot().config.default_model.as_ref())
            .cloned();
        if let Some(id) = id {
            self.control.await_model(&id, cancel).await?;
            self.reload()?;
        }
        Ok(())
    }
}
