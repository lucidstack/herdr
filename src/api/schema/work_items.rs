use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemTarget {
    pub item_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemChooseParams {
    pub item_id: String,
    pub choice_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemLinkParams {
    pub item_id: String,
    /// Existing workspace that becomes the item's workspace.
    pub workspace_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemHideParams {
    pub item_id: String,
    /// Hide for this many seconds. Absent dismisses the item until it is requested again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snooze_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemSearchParams {
    /// Configured source to search, e.g. `github` or `jira`.
    pub source_id: String,
    /// Query in the tracker's own syntax: GitHub search syntax or JQL.
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemTicketTarget {
    pub source_id: String,
    /// Ticket key in the tracker's form, e.g. `TECH-123` or `owner/repo#12`.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemPickNextStartParams {
    /// Configured source whose "Pick next" discovery workspace is started or reused.
    pub source_id: String,
    /// Optional context for the agent; sent as the initial brief, or as a follow-up brief
    /// when a discovery workspace is already open. May be empty.
    #[serde(default)]
    pub context: String,
}

/// One ticket as its tracker reports it, whether or not it is in the inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemTicketInfo {
    pub key: String,
    pub title: String,
    /// The tracker's status name, e.g. `In Progress`, `open` or `merged`.
    pub status: String,
    /// Whether the tracker considers the ticket finished (closed, merged, done).
    #[serde(default)]
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    pub updated_at: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemTicketComment {
    pub author: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemPhase {
    /// No choice has been made yet.
    Pending,
    /// The user chose an action outside Herdr and the source still reports the item.
    AwaitingExternal,
    /// A local workspace is provisioned or being provisioned.
    Local,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkItemChoiceAction {
    OpenUrl {
        url: String,
    },
    ProvisionWorkspace,
    /// The source does it on the server, e.g. merging a pull request.
    Perform,
    /// Herdr sends a follow-up brief to the agent in the item's workspace and focuses it.
    BriefAgent,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemChoiceInfo {
    pub choice_id: String,
    pub label: String,
    /// One line explaining what the choice does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub action: WorkItemChoiceAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// Shown before a choice that cannot be undone runs; the user confirms it a second time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStep {
    /// The worktree or scratch directory is ready.
    Checkout,
    AgentBrief,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStepStatus {
    Pending,
    Running,
    Done,
    Skipped,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemStepInfo {
    pub step: WorkItemStep,
    pub label: String,
    pub status: WorkItemStepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemProvisioningInfo {
    pub steps: Vec<WorkItemStepInfo>,
    pub finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemInfo {
    pub item_id: String,
    pub source_id: String,
    pub context: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    pub phase: WorkItemPhase,
    pub seen: bool,
    #[serde(default)]
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Hidden until the source requests the item again.
    #[serde(default)]
    pub dismissed: bool,
    /// Hidden until this Unix time in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<u64>,
    #[serde(default)]
    pub choices: Vec<WorkItemChoiceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_choice_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioning: Option<WorkItemProvisioningInfo>,
    /// A provider's "Pick next" discovery row rather than a tracked ticket. `context` holds
    /// the last text sent to it.
    #[serde(default)]
    pub is_pick_next: bool,
    /// Set when you work on the item (it has a workspace) but its tracker lags behind:
    /// the ticket is not assigned to you, or still in a to-do status. One line for the
    /// user, e.g. "You're working on this, but it isn't assigned to you".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_reminder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemSourceInfo {
    pub source_id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Persisted "Pick next" memory, so the dialog can pre-fill without depending on item
/// recency: which provider was used last, and each provider's last context text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemPickNextInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_source_id: Option<String>,
    #[serde(default)]
    pub last_context: HashMap<String, String>,
}
