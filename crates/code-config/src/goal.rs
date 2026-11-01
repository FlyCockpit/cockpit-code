//! Frozen policy for autonomous criteria contracts.
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GoalConfig {
    pub limits: GoalLimits,
    pub no_progress_turns: u64,
    pub repeated_gap_rounds: u64,
    pub review_panel: bool,
    pub max_review_rounds: u64,
    pub usage_resume: UsageResume,
}
impl Default for GoalConfig {
    fn default() -> Self {
        Self {
            limits: GoalLimits::default(),
            no_progress_turns: 3,
            repeated_gap_rounds: 3,
            review_panel: true,
            max_review_rounds: 3,
            usage_resume: UsageResume::default(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GoalLimits {
    pub max_usd: Option<f64>,
    pub max_minutes: Option<f64>,
    pub max_tokens: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsageResume {
    pub max: u64,
    pub backoff_secs: u64,
}
impl Default for UsageResume {
    fn default() -> Self {
        Self {
            max: 3,
            backoff_secs: 30,
        }
    }
}
