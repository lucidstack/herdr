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
    /// The item waits on someone else, e.g. reviewers asked to review again.
    pub waiting: bool,
    /// Whether the tracker reports this ticket finished (closed, merged, done…). Only acted
    /// on for hand-added items; ignored for others.
    pub done: bool,
}

/// One ticket's full detail, plus enough to insert it into the inbox like a polled arrival.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TicketDetail {
    pub ticket: crate::api::schema::WorkItemTicketInfo,
    pub description: String,
    /// Oldest first.
    pub comments: Vec<crate::api::schema::WorkItemTicketComment>,
    pub source_item: SourceItem,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemChoices {
    pub choices: Vec<WorkItemChoiceInfo>,
    pub default_choice_id: Option<String>,
}

/// A worktree created through Herdr's worktree support on a local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeSpec {
    /// Existing clone the worktree is added to.
    pub repo_path: PathBuf,
    /// Remote name or URL that serves `fetch_refspec`.
    pub remote: String,
    pub fetch_refspec: String,
    /// More refspecs fetched with `fetch_refspec`, e.g. the pull request's base branch.
    pub extra_fetch_refspecs: Vec<String>,
    /// Fetched ref a new branch starts from.
    pub base_ref: String,
    /// Local branch the worktree is on.
    pub branch: String,
    /// Work on `branch` when it already exists locally (the pull request's own branch).
    /// Otherwise an existing `branch` is kept and a suffixed branch continues from it.
    pub reuse_branch: bool,
    /// Continue an existing worktree or local branch whose name contains this issue key
    /// instead of creating `branch`.
    pub adopt_branch_for: Option<String>,
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
    /// A Herdr-managed Git worktree of the change.
    Worktree(WorktreeSpec),
    /// No checkout: only a downloaded file for reading.
    Download(DownloadSpec),
    /// No checkout and no file: an empty directory for an agent that only reads the
    /// tracker and other clones, e.g. "Pick next" discovery.
    Scratch(PathBuf),
}

/// Tabs and agent of a provisioned workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceLayout {
    /// Agent kind started in the first tab; empty starts none.
    pub agent: String,
    /// Extra arguments the agent is started with.
    pub agent_args: Vec<String>,
    pub editor_command: String,
    pub lazygit_command: String,
    /// Diff viewer for download workspaces; `{file}` is the downloaded file.
    pub diff_command: String,
    /// Replaces the Git tab of a worktree when `{plugin:ID}` resolves; `{base}` is already filled in.
    pub review_command: String,
}

/// Everything needed to provision a local workspace for an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvisionPlan {
    pub source: WorkspaceSource,
    pub workspace_label: String,
    pub agent_name_hint: String,
    /// Prompt delivered to the agent once it is ready.
    pub brief: String,
    pub layout: WorkspaceLayout,
    /// Delete the review branch when its worktree is removed.
    pub delete_branch: bool,
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
    /// Pure: whether the item's workspace is removed once the source stops reporting it.
    fn remove_on_resolved(&self, item: &WorkItem) -> bool;
    /// Pure: notification text when an item arrives or is requested again.
    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>);
    /// Blocking; background thread only. Searches the tracker with its own query syntax
    /// (JQL for Jira, GitHub search syntax for GitHub), read-only.
    fn search(&self, query: &str) -> Result<Vec<crate::api::schema::WorkItemTicketInfo>, String>;
    /// Blocking; background thread only. Fetches one ticket by its tracker key (e.g.
    /// `TECH-123` or `owner/repo#12`); `None` when the key does not exist.
    fn fetch(&self, key: &str) -> Result<Option<TicketDetail>, String>;
    /// Pure: plan for this provider's shared "Pick next" discovery workspace (a scratch
    /// directory; no checkout), briefed with `context` (may be empty) to investigate the
    /// tracker read-only and recommend what to work on next.
    fn pick_next_plan(
        &self,
        context: &str,
        worktree_directory: &Path,
    ) -> Result<ProvisionPlan, String>;
    /// Blocking; background thread only. Carries out a choice whose action is `Perform`
    /// and returns a one-line result for the user.
    fn perform(&self, _item: &WorkItem, choice_id: &str) -> Result<String, String> {
        Err(format!("choice {choice_id} cannot be carried out here"))
    }
    /// Pure: text sent to the agent in the item's workspace for a choice whose action is
    /// `BriefAgent`.
    fn follow_up_brief(&self, _item: &WorkItem, choice_id: &str) -> Result<String, String> {
        Err(format!("choice {choice_id} does not brief an agent"))
    }
}
