use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceAddParams {
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
    pub label: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceAddResult {
    pub service_id: u64,
    pub workspace_id: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceListResult {
    pub services: Vec<ServiceListEntry>,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceRemoveResult {}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceSnapshot {
    pub id: u64,
    pub label: String,
    pub url: String,
    pub source: String,
}
