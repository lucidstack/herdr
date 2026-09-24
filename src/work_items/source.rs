use std::path::{Path, PathBuf};
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

/// How to obtain a local checkout of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckoutSpec {
    /// Existing clone the worktree is added to.
    pub repo_path: PathBuf,
    /// Remote name or URL that serves `fetch_refspec`.
    pub remote: String,
    pub fetch_refspec: String,
    /// Local ref checked out detached.
    pub checkout_ref: String,
    pub checkout_path: PathBuf,
}

/// A scratch directory holding one file produced by a command, e.g. a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownloadSpec {
    pub directory: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Name of the file inside `directory` that receives the command's output.
    pub file_name: String,
}

/// What the provisioned workspace is rooted in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceSource {
    /// A detached Git worktree of the change.
    Worktree(CheckoutSpec),
    /// No checkout: only a downloaded file for reading.
    Download(DownloadSpec),
}

impl WorkspaceSource {
    /// Directory the workspace's panes start in.
    pub(crate) fn directory(&self) -> &Path {
        match self {
            Self::Worktree(spec) => &spec.checkout_path,
            Self::Download(spec) => &spec.directory,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerPlan {
    pub command: String,
    pub port: Option<u16>,
}

/// Everything needed to provision a local workspace for an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvisionPlan {
    pub source: WorkspaceSource,
    pub workspace_label: String,
    pub agent_name_hint: String,
    /// Prompt delivered to the agent once it is ready.
    pub brief: String,
    pub install_command: Option<String>,
    pub server: Option<ServerPlan>,
    /// Shown on the server step when `server` is `None`.
    pub server_skip_reason: String,
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
    /// Pure: plan for `choice_id`, whose action provisions a workspace.
    fn provision_plan(
        &self,
        item: &WorkItem,
        choice_id: &str,
        worktree_directory: &Path,
    ) -> Result<ProvisionPlan, String>;
    /// Pure: notification text when an item arrives or is requested again.
    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>);
}
