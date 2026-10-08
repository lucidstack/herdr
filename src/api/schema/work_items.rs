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
    /// Ids of the choice's options to switch on, from `WorkItemChoiceInfo.options`. Absent:
    /// each option's `default`. An id the choice does not offer is an `unknown_option` error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    /// Model the choice's agent starts with, one of `WorkItemChoiceInfo.agent.models`, in
    /// place of the one its configuration names. Absent: as configured. A choice without
    /// `agent`, or a model it does not list, is an `unknown_model` error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Effort level the choice's agent starts with, one of `WorkItemChoiceInfo.agent.efforts`,
    /// in place of the one its configuration names. Absent: as configured. A choice without
    /// `agent`, or an effort it does not list, is an `unknown_effort` error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
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

/// A repository or tracker project of a source, e.g. `owner/name` on GitHub or `APP` in
/// Jira. Items of an ignored one stay out of this session's inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemProject {
    pub source_id: String,
    pub project: String,
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
pub struct WorkItemImageParams {
    /// Configured source whose credentials fetch the image, e.g. `github`.
    pub source_id: String,
    /// The image's address as a ticket's description or comment gives it, e.g.
    /// `https://github.com/user-attachments/assets/<id>`.
    pub url: String,
}

/// Whether an image a ticket points at could be fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemImageStatus {
    /// `media_type`, `byte_count` and `data` hold the image.
    Available,
    /// The tracker has nothing at this address, or nothing you may read.
    NotFound,
    /// The file is larger than 10 MiB; `byte_count` says how large, and no `data` is sent.
    TooLarge,
    /// The file is not a PNG, JPEG, GIF or WebP image, e.g. a video.
    NotAnImage,
    /// The source does not fetch this address: for GitHub, anything other than an image
    /// attachment on github.com. A client fetches such an address itself.
    UnsupportedUrl,
    #[serde(other)]
    Unknown,
}

/// One image a ticket points at, fetched with the source's credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemImageInfo {
    pub source_id: String,
    pub url: String,
    pub status: WorkItemImageStatus,
    /// The image's type, e.g. "image/png". Present when `status` is `available`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// The file's size in bytes. Present when `status` is `available` or `too_large`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_count: Option<u64>,
    /// The image's bytes in standard base64. Present when `status` is `available`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
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
    /// Switches the user sets before running the choice. `work_item.choose` takes the ones
    /// switched on. Omitted when the choice has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<WorkItemChoiceOptionInfo>,
    /// The models and effort levels `work_item.choose` can start the choice's agent with.
    /// Omitted when the choice starts no agent, or one Herdr passes neither to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<WorkItemChoiceAgentInfo>,
}

/// How the agent a choice starts can be started, e.g. Claude Code with `--model` and
/// `--effort`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemChoiceAgentInfo {
    /// Model names the agent understands, e.g. "opus".
    pub models: Vec<String>,
    /// Effort levels, least first, e.g. "low".
    pub efforts: Vec<String>,
    /// The model the agent's configured arguments name; omitted when they name none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_model: Option<String>,
    /// The effort level the agent's configured arguments name; omitted when they name none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_effort: Option<String>,
}

/// A switch of a choice, e.g. whether reviewing a pull request creates a worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemChoiceOptionInfo {
    pub option_id: String,
    pub label: String,
    /// One line explaining what the switch does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether it is on when `work_item.choose` names no options.
    pub default: bool,
}

/// Starts the id of a choice a ticket offers on behalf of the pull request linked to it. The
/// rest is the id the pull request's own inbox item gives that choice, so
/// `pull_request:merge_squash` is that item's `merge_squash`. Clients treat choice ids as
/// opaque; one that also lists the pull request item can use this to skip choices the ticket
/// already offers.
pub const WORK_ITEM_PULL_REQUEST_CHOICE_PREFIX: &str = "pull_request:";

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
    /// Unix time in seconds when every step had finished. Present once `finished` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemInfo {
    pub item_id: String,
    pub source_id: String,
    pub context: String,
    /// Where the ticket stands in its tracker, e.g. "Selected for Development · unassigned".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker_state: Option<String>,
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
    /// The `Perform` choice the source is carrying out right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_choice_id: Option<String>,
    /// How the last `Perform` choice on this item ended, until another one starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_outcome: Option<WorkItemActionOutcome>,
    /// The pull request opened from the branch of the item's workspace, for items that
    /// are not pull requests themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_pull_request: Option<WorkItemPullRequestInfo>,
    /// Where the pull request this item is stands, for items that are pull requests
    /// themselves, e.g. a review request: draft, awaiting review, approved, merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_pull_request: Option<WorkItemPullRequestInfo>,
    /// The tracker ticket this item's title names, for items of another source, e.g. the
    /// Jira issue a pull request titled "[TECH-12] Fix login" is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_ticket: Option<WorkItemLinkedTicketInfo>,
    /// The item whose linked pull request this item is: shown with that item instead of
    /// on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folded_into: Option<String>,
    /// Set while the item needs you, with the kind of need and why. Absent when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<AttentionInfo>,
    /// The repository or tracker project the item belongs to, e.g. `owner/name` or `APP`:
    /// what `work_item.ignore_project` keeps out of this session's inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// Why something needs you. Ordered from most to least urgent.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    /// An agent waits on you, e.g. for a permission or a question.
    Blocked,
    /// An agent has been running one shell command for at least `work_items.stuck_after_seconds`.
    Stuck,
    /// Provisioning a workspace or carrying out a choice failed.
    Failed,
    /// An agent finished its turn and its pane has been left alone since.
    Finished,
    /// A reviewer requested changes on your pull request.
    ChangesRequested,
    /// Checks are failing on your pull request.
    ChecksFailing,
    /// Your pull request is approved and waits for you to merge it.
    ReadyToMerge,
    /// Your pull request is merged, but its ticket is still open in the tracker.
    ReadyToClose,
    /// Newly arrived and not acted on yet: a review request, an assignment, a mention.
    New,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttentionInfo {
    pub kind: AttentionKind,
    /// One line for the user, e.g. "Allow running bin/rails db:migrate?".
    pub reason: String,
    /// The agent's pane, for `blocked`, `stuck` and `finished`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// Unix time in seconds when it entered this kind.
    pub since: u64,
}

/// An agent in a pane outside every work item's workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentAttentionInfo {
    pub pane_id: String,
    pub workspace_id: String,
    /// The workspace's name.
    pub workspace_label: String,
    /// The agent, e.g. "claude".
    pub agent: String,
    pub agent_status: super::AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<AttentionInfo>,
}

/// A pull request's state: one found for the branch an item is worked on, or the one an
/// item is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemPullRequestInfo {
    /// Source hosting it, e.g. "github".
    pub source_id: String,
    /// Repository as owner/name.
    pub repo: String,
    pub number: u64,
    pub url: String,
    /// Open and still a draft: it waits for you to mark it ready for review.
    pub is_draft: bool,
    /// One line for the user, e.g. "draft", "awaiting review · checks running", "merged".
    pub status: String,
    /// The GitHub stack it belongs to, when it is part of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<WorkItemPullRequestStackInfo>,
}

/// A stack of pull requests, each based on the one below it, that a pull request is part of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemPullRequestStackInfo {
    /// Number of the stack in its repository.
    pub number: u64,
    /// Branch the bottom of the stack targets, e.g. "main".
    pub base: String,
    /// This pull request's place in the stack, 1 being the bottom.
    pub position: u32,
    /// Every pull request in the stack, bottom first, merged ones included.
    pub entries: Vec<WorkItemPullRequestStackEntry>,
}

/// One pull request of a stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemPullRequestStackEntry {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// Its head branch.
    pub head: String,
    /// "open", "merged" or "closed".
    pub state: String,
}

impl WorkItemPullRequestStackEntry {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

/// A tracker ticket named in the title of an item from another source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemLinkedTicketInfo {
    /// Source tracking it, e.g. "jira".
    pub source_id: String,
    /// Ticket key in the tracker's form, e.g. `TECH-123`.
    pub key: String,
    pub url: String,
    /// Where the ticket stands in its tracker, e.g. "In Progress · Ada".
    pub tracker_state: String,
}

/// How a `Perform` choice ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemActionOutcome {
    pub choice_id: String,
    pub succeeded: bool,
    /// What changed, e.g. "TECH-7 is yours and In Progress", or why it failed.
    pub message: String,
}

/// A local clone mapped by a work-item source (`[work_items.github] repos`,
/// `[work_items.jira] projects`), offered as a way to reach its main checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemRepositoryInfo {
    /// Absolute path of the clone on the server's machine.
    pub path: String,
    /// Short name, e.g. the directory name.
    pub label: String,
    /// The open workspace on the clone's main checkout (its "home"), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
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
