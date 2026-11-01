//! Inherited fan-out budgets and conjunctive model policy layers.
use crate::{Control, schema::EngineError, sessions::Journal};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelPolicy {
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
}
impl ModelPolicy {
    pub(crate) fn validate(&self) -> Result<(), EngineError> {
        for pattern in self.allow.iter().flatten().chain(self.deny.iter()) {
            globset::Glob::new(pattern)
                .map_err(|e| EngineError::bad(format!("invalid model pattern {pattern}: {e}")))?;
        }
        Ok(())
    }
    pub(crate) fn permits(&self, id: &str) -> bool {
        let matches =
            |p: &String| globset::Glob::new(p).is_ok_and(|g| g.compile_matcher().is_match(id));
        self.allow.as_ref().is_none_or(|a| a.iter().any(matches)) && !self.deny.iter().any(matches)
    }
}
pub(crate) fn permits(layers: &[ModelPolicy], id: &str) -> bool {
    layers.iter().all(|p| p.permits(id))
}
pub(crate) fn depth(j: &Journal, id: &str) -> usize {
    let mut next = j.agents.get(id).and_then(|a| a.parent_id.as_deref());
    let mut depth = 0;
    while let Some(id) = next {
        depth += 1;
        next = j.agents.get(id).and_then(|a| a.parent_id.as_deref());
    }
    depth
}
pub(crate) fn remaining(j: &Journal, id: &str) -> Option<u64> {
    let mut next = Some(id);
    let mut budget: Option<u64> = None;
    while let Some(id) = next {
        if let Some(cap) = j.executions.get(id).and_then(|e| e.max_descendants) {
            let left = cap.saturating_sub(j.descendant_launches.get(id).copied().unwrap_or(0));
            budget = Some(budget.map_or(left, |n| n.min(left)));
        }
        next = j.agents.get(id).and_then(|a| a.parent_id.as_deref());
    }
    budget
}
impl Control {
    pub(crate) fn model_allowed(&self, agent: &str, id: &str) -> bool {
        self.journal
            .lock()
            .unwrap()
            .executions
            .get(agent)
            .is_none_or(|e| permits(&e.model_policy, id))
    }
}
