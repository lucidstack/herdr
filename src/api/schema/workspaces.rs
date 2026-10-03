use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::AgentStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceCreateParams {
    /// Workspace whose focused pane supplies the `follow` cwd policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceCloseParams {
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub close_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceRenameParams {
    pub workspace_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceMoveParams {
    pub workspace_id: String,
    pub insert_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceMoveBlockParams {
    pub workspace_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceReportMetadataParams {
    pub workspace_id: String,
    pub source: String,
    #[schemars(schema_with = "super::common::metadata_token_patch_schema")]
    pub tokens: HashMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 86_400_000))]
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub tab_count: usize,
    pub active_tab_id: String,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[schemars(schema_with = "super::common::metadata_token_values_schema")]
    pub tokens: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorkspaceWorktreeInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceWorktreeInfo {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub checkout_path: String,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceDiffParams {
    pub workspace_id: String,
    /// Only this file, as `files[].path` (or `old_path`) names it, with a 2 MiB patch
    /// allowance instead of 128 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Leave out every patch, for the counts alone.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub summary_only: bool,
}

/// Whether a workspace's changes could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceDiffStatus {
    /// `files` holds the changes.
    Available,
    /// The workspace has no directory.
    NoDirectory,
    /// The workspace's directory is not in a Git work tree.
    NotARepository,
    /// Git failed or timed out; `error` carries its message.
    Unreadable,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceDiffFileStatus {
    Added,
    Modified,
    Deleted,
    /// Moved, perhaps with changes; `old_path` names where it was.
    Renamed,
    /// `old_path` names the file it was copied from.
    Copied,
    /// Turned from a file into a symbolic link or the other way round.
    TypeChanged,
    /// Not tracked by Git and not ignored.
    Untracked,
    #[serde(other)]
    Unknown,
}

/// What `workspace.diff` compares the working tree with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceDiffBase {
    /// The branch the base was found from, e.g. "origin/main".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
    /// The merge-base of `HEAD` with that branch.
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceDiffFile {
    /// Relative to `directory`.
    pub path: String,
    /// Present when `status` is `renamed` or `copied`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: WorkspaceDiffFileStatus,
    /// Lines added, absent for binary files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additions: Option<u64>,
    /// Lines deleted, absent for binary files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    #[serde(default)]
    pub binary: bool,
    /// The file's unified diff hunks, from its first `@@` line, without the `diff --git`,
    /// `index`, `---` and `+++` headers. Absent for binary files, with `summary_only`, and
    /// when left out for size, which `patch_truncated` says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// The patch was left out because it, or the reply's patches before it, were too large.
    #[serde(default)]
    pub patch_truncated: bool,
}

/// What changed in a workspace's checkout: the working tree, with committed, staged,
/// unstaged and untracked changes, against `base`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceDiffInfo {
    pub workspace_id: String,
    pub status: WorkspaceDiffStatus,
    /// The checkout's top-level directory, which `files` paths are relative to. When the
    /// workspace is not in a work tree, its own directory; absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// The checked-out branch; `null` when `HEAD` is detached.
    #[serde(default)]
    pub branch: Option<String>,
    /// `null` when no base branch was found: `files` then holds only the changes not yet
    /// committed.
    #[serde(default)]
    pub base: Option<WorkspaceDiffBase>,
    /// The commit `HEAD` points at; `null` on a branch with no commit yet.
    #[serde(default)]
    pub head: Option<String>,
    /// Lines added across the listed text files.
    #[serde(default)]
    pub additions: u64,
    /// Lines deleted across the listed text files.
    #[serde(default)]
    pub deletions: u64,
    /// More files changed than are listed.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub files: Vec<WorkspaceDiffFile>,
    /// Git's message when `status` is `unreadable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
