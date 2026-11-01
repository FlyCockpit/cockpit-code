//! Overrides change presentation and availability, never parameter shape.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    #[default]
    Full,
    Core,
}
/// Global profile; D111 global overrides remain in tools.json.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profiles {
    pub profile: Profile,
}
/// Existing per-tool overrides plus an independent native exposure policy.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    #[serde(flatten)]
    pub overrides: BTreeMap<String, ToolOverride>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ToolOverride {
    pub name: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub parameters: BTreeMap<String, ParameterOverride>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParameterOverride {
    pub description: Option<String>,
}
impl ToolOverride {
    pub fn overlay(&mut self, higher: &Self) {
        if higher.name.is_some() {
            self.name = higher.name.clone();
        }
        if higher.description.is_some() {
            self.description = higher.description.clone();
        }
        if higher.enabled.is_some() {
            self.enabled = higher.enabled;
        }
        for (name, param) in &higher.parameters {
            if param.description.is_some() {
                self.parameters.insert(name.clone(), param.clone());
            }
        }
    }
}
