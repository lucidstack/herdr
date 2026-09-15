use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Registers or updates a service. `workspace_id` wins over `pane_id`; when
/// only `pane_id` is given the pane's workspace owns the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceAddParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub label: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceListEntry {
    pub workspace_id: String,
    pub id: u64,
    pub label: String,
    pub url: String,
    pub source: String,
    pub liveness: ServiceLivenessWire,
}

/// Removes a service by `id` or `label` (one is required) from the workspace
/// resolved like [`ServiceAddParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceRemoveParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServiceLivenessWire {
    #[default]
    Unknown,
    Up,
    Down,
}

impl From<crate::service::ServiceLiveness> for ServiceLivenessWire {
    fn from(liveness: crate::service::ServiceLiveness) -> Self {
        match liveness {
            crate::service::ServiceLiveness::Unknown => ServiceLivenessWire::Unknown,
            crate::service::ServiceLiveness::Up => ServiceLivenessWire::Up,
            crate::service::ServiceLiveness::Down => ServiceLivenessWire::Down,
        }
    }
}

/// Persisted form of a service; liveness is never persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceSnapshot {
    pub id: u64,
    pub label: String,
    pub url: String,
    pub source: String,
}
