//! Language server overrides and resolved server definitions.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspServer {
    pub command: String,
    pub args: Vec<String>,
    pub file_globs: Vec<String>,
    pub root_markers: Vec<String>,
}

/// Partial user override; omitted fields inherit the shipped default.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspServerOverride {
    pub enabled: Option<bool>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub file_globs: Option<Vec<String>>,
    pub root_markers: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AfterWrite {
    #[serde(rename = "errors")]
    Errors,
    #[default]
    #[serde(rename = "errors+warnings")]
    ErrorsWarnings,
    #[serde(rename = "off")]
    Off,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspOverrides {
    pub after_write: Option<AfterWrite>,
    pub after_write_ms: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspConfig {
    pub after_write: AfterWrite,
    pub after_write_ms: u64,
    #[serde(flatten)]
    pub servers: std::collections::BTreeMap<String, LspServerOverride>,
}
impl Default for LspConfig {
    fn default() -> Self {
        Self {
            after_write: AfterWrite::ErrorsWarnings,
            after_write_ms: 1500,
            servers: Default::default(),
        }
    }
}
impl std::ops::Deref for LspConfig {
    type Target = std::collections::BTreeMap<String, LspServerOverride>;
    fn deref(&self) -> &Self::Target {
        &self.servers
    }
}
impl std::ops::DerefMut for LspConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.servers
    }
}
impl<'a> IntoIterator for &'a LspConfig {
    type Item = (&'a String, &'a LspServerOverride);
    type IntoIter = std::collections::btree_map::Iter<'a, String, LspServerOverride>;
    fn into_iter(self) -> Self::IntoIter {
        self.servers.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_server_fields_round_trip() {
        let value = serde_json::json!({"command":"server", "args":["--stdio"],
            "fileGlobs":["*.rs"], "rootMarkers":["Cargo.toml"]});
        let server: LspServer = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(server).unwrap(), value);
        let partial: LspServerOverride = serde_json::from_value(value).unwrap();
        assert_eq!(partial.file_globs, Some(vec!["*.rs".into()]));
        assert_eq!(partial.root_markers, Some(vec!["Cargo.toml".into()]));
        assert_eq!(partial.enabled, None);
    }

    #[test]
    fn unknown_lsp_fields_receive_normal_unknown_field_treatment() {
        for name in ["file_globs", "globs", "root_markers", "unknown"] {
            let value = serde_json::json!({name:["ignored"]});
            let server: LspServer = serde_json::from_value(value.clone()).unwrap();
            let partial: LspServerOverride = serde_json::from_value(value).unwrap();
            assert!(server.file_globs.is_empty());
            assert!(server.root_markers.is_empty());
            assert!(partial.file_globs.is_none());
            assert!(partial.root_markers.is_none());
        }
    }
}
