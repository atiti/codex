//! Serializable capability overrides for custom Responses-compatible providers.
//!
//! Unspecified capabilities inherit the provider's existing defaults. Model and
//! turn settings may further restrict the capabilities declared here.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

/// Overrides for the optional API features supported by a custom provider.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderCapabilities {
    /// Context windows by model slug, independent of account-specific model metadata.
    /// Missing models and nonpositive values retain their catalog metadata.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context_windows: BTreeMap<String, i64>,
    /// Whether hosted web search may access the live web.
    pub external_web_access: Option<bool>,
    /// Remote context-compaction protocol; omission preserves provider defaults.
    pub remote_compaction: Option<RemoteCompactionSupport>,
}

/// Remote context-compaction protocols supported by a model provider.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCompactionSupport {
    /// Use local compaction instead of a provider compaction protocol.
    Unsupported,
    /// Send `compaction_trigger` items over the Responses endpoint.
    V2,
}
