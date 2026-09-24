use std::time::Duration;

use crate::api::schema::WorkItemChoiceInfo;

use super::state::WorkItem;

/// One item as reported by a source poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceItem {
    pub external_id: String,
    pub title: String,
    pub context: String,
    pub author: Option<String>,
    pub url: String,
    pub updated_at: String,
}

/// Result of the cheap background preparation of one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedItem {
    /// Source-opaque details; only the source interprets them.
    pub detail: Option<serde_json::Value>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemChoices {
    pub choices: Vec<WorkItemChoiceInfo>,
    pub default_choice_id: Option<String>,
}

/// An external system that produces work items.
pub(crate) trait WorkItemSource: Send + Sync {
    /// Stable identifier used as the item-key prefix, e.g. "github".
    fn id(&self) -> &str;
    /// Human-readable label shown in errors, e.g. "GitHub".
    fn label(&self) -> &str;
    fn poll_interval(&self) -> Duration;
    /// Blocking; always called on a background thread.
    fn poll(&self) -> Result<Vec<SourceItem>, String>;
    /// Blocking, cheap preparation (details, fetch). Background thread only.
    fn prepare(&self, item: &SourceItem) -> PreparedItem;
    /// Pure: choices offered for an item.
    fn choices(&self, item: &WorkItem) -> ItemChoices;
    /// Pure: notification text when an item arrives or is requested again.
    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>);
}
