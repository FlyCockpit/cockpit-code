use crate::{Session, model::Model, schema::EngineError, sessions::SessionSettings};
use code_config::{PermissionMode, Snapshot, Transport};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock, Weak},
};

type PolicyCell = Mutex<Option<Exclusive>>;
type Policy = Arc<PolicyCell>;
type PolicyRegistry = Mutex<BTreeMap<String, Weak<PolicyCell>>>;
fn policies() -> &'static PolicyRegistry {
    static POLICIES: OnceLock<PolicyRegistry> = OnceLock::new();
    POLICIES.get_or_init(Mutex::default)
}
pub(crate) fn register_policy(session: String, policy: &Policy) {
    let mut policies = policies().lock().unwrap();
    policies.retain(|_, p| p.strong_count() != 0);
    policies.insert(session, Arc::downgrade(policy));
}
pub(crate) fn check_request(session: &str, kind: &str, model: &str) -> Result<(), EngineError> {
    let policy = policies()
        .lock()
        .unwrap()
        .get(session)
        .and_then(Weak::upgrade);
    if let Some(policy) = policy
        && let Some(exclusive) = policy.lock().unwrap().as_ref()
    {
        let allowed = if kind == "safety" {
            exclusive.safety_model()
        } else {
            Some(exclusive.model.as_str())
        };
        if allowed != Some(model) {
            return Err(EngineError::bad(
                "model request refused by the current exclusive setting",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ExclusiveSafety {
    SameModel,
    Model(String),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Exclusive {
    pub model: String,
    pub safety: Option<ExclusiveSafety>,
    #[serde(default)]
    pub block_network: bool,
}
impl Exclusive {
    pub fn safety_model(&self) -> Option<&str> {
        match &self.safety {
            None => None,
            Some(ExclusiveSafety::SameModel) => Some(&self.model),
            Some(ExclusiveSafety::Model(id)) => Some(id),
        }
    }
}
pub(crate) fn safety(settings: &SessionSettings) -> Option<String> {
    if let Some(ex) = &settings.exclusive {
        ex.safety_model().map(str::to_owned)
    } else {
        settings.safety_model.clone()
    }
}
pub(crate) fn enabled_mcps(settings: &SessionSettings, snapshot: &Snapshot) -> BTreeSet<String> {
    settings
        .mcps
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|name| {
            !settings
                .exclusive
                .as_ref()
                .is_some_and(|ex| ex.block_network)
                || snapshot
                    .mcps
                    .get(name)
                    .and_then(|m| m.connection())
                    .is_some_and(|m| matches!(m.transport, Transport::Stdio))
        })
        .collect()
}
pub(crate) fn check_model(settings: &SessionSettings, model: &Model) -> Result<(), EngineError> {
    if let Some(ex) = &settings.exclusive
        && ex.model != format!("{}/{}", model.provider_id, model.model_id)
    {
        return Err(EngineError::bad(
            "model checkpoint is unavailable under the current exclusive model; run the asker on the exclusive model first",
        ));
    }
    Ok(())
}
pub(crate) fn check_mode(
    settings: &SessionSettings,
    mode: PermissionMode,
) -> Result<(), EngineError> {
    if matches!(mode, PermissionMode::Auto | PermissionMode::Unattended)
        && settings
            .exclusive
            .as_ref()
            .is_some_and(|ex| ex.safety.is_none())
    {
        return Err(EngineError::bad(
            "exclusive mode has no safety model; use manual or yolo",
        ));
    }
    Ok(())
}
impl Session {
    pub fn exclusive(&self) -> Option<Exclusive> {
        self.settings().exclusive
    }
    pub fn set_exclusive(&self, exclusive: Option<Exclusive>) -> Result<(), EngineError> {
        let snapshot = self.config.lock().unwrap().clone();
        if let Some(ex) = &exclusive {
            crate::permissions::validate_model(&snapshot, &ex.model)?;
            if let Some(id) = ex.safety_model() {
                crate::permissions::validate_model(&snapshot, id)?;
            }
        }
        let mut changes = vec![("exclusive".into(), serde_json::json!(exclusive))];
        if let Some(ex) = &exclusive {
            changes.push(("model".into(), serde_json::json!(ex.model)));
            changes.push(("effort".into(), serde_json::Value::Null));
            if ex.safety.is_none() {
                let mode = self
                    .settings()
                    .mode
                    .unwrap_or(snapshot.config.permissions.mode);
                if matches!(mode, PermissionMode::Auto | PermissionMode::Unattended) {
                    changes.push(("mode".into(), serde_json::json!(PermissionMode::Manual)));
                }
            }
        }
        self.control.settings_batch(changes)?;
        self.control
            .mcp
            .set_enabled(&enabled_mcps(&self.settings(), &snapshot));
        self.control.changed.notify_waiters();
        Ok(())
    }
    pub async fn mcp_health_check(&self) -> Vec<crate::mcp::McpStatus> {
        let names = self
            .control
            .mcp
            .statuses()
            .into_iter()
            .filter(|s| s.enabled)
            .map(|s| s.name)
            .collect::<Vec<_>>();
        futures_util::future::join_all(
            names
                .iter()
                .map(|name| self.control.mcp.catalog(name, &self.control.health_cancel)),
        )
        .await;
        self.control.mcp.statuses()
    }
}
