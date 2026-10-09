use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo};

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
    /// Where the ticket stands in its tracker, e.g. "Selected for Development · unassigned".
    pub tracker_state: Option<String>,
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

/// An image a ticket points at, as its source fetched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TicketImage {
    Available {
        media_type: &'static str,
        bytes: Vec<u8>,
    },
    NotFound,
    TooLarge {
        byte_count: u64,
    },
    NotAnImage,
    UnsupportedUrl,
}

/// The most `work_item.image` sends, as for `agent.image`.
pub(crate) const MAX_TICKET_IMAGE_BYTES: usize = 10 * 1024 * 1024;

impl TicketImage {
    /// `bytes` as fetched: too large, not an image, or the image with its type.
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        if bytes.len() > MAX_TICKET_IMAGE_BYTES {
            return Self::TooLarge {
                byte_count: bytes.len() as u64,
            };
        }
        match image_media_type(&bytes) {
            Some(media_type) => Self::Available { media_type, bytes },
            None => Self::NotAnImage,
        }
    }
}

/// The type of a PNG, JPEG, GIF or WebP file, from its first bytes.
fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemChoices {
    pub choices: Vec<WorkItemChoiceInfo>,
    pub default_choice_id: Option<String>,
}

/// The ids of the options of `choice` that are switched on. `requested` names them; without
/// it they are the options on by default. Fails with the first id the choice does not offer.
pub(crate) fn switched_on_options<'a>(
    choice: &WorkItemChoiceInfo,
    requested: Option<&'a [String]>,
) -> Result<Vec<String>, &'a str> {
    let Some(requested) = requested else {
        return Ok(choice
            .options
            .iter()
            .filter(|option| option.default)
            .map(|option| option.option_id.clone())
            .collect());
    };
    if let Some(unknown) = requested
        .iter()
        .find(|id| !choice.options.iter().any(|option| option.option_id == **id))
    {
        return Err(unknown);
    }
    Ok(choice
        .options
        .iter()
        .filter(|option| requested.contains(&option.option_id))
        .map(|option| option.option_id.clone())
        .collect())
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
    /// Tabs opened after the agent's tab when the workspace has a checkout.
    pub tabs: Vec<crate::config::WorkspaceTabConfig>,
    /// Diff viewer for download workspaces; `{file}` is the downloaded file.
    pub diff_command: String,
}

/// Everything needed to provision a local workspace for an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvisionPlan {
    pub source: WorkspaceSource,
    pub workspace_label: String,
    pub agent_name_hint: String,
    /// Prompt delivered to the agent once it is ready.
    pub brief: String,
    /// Prompt sent before the brief to switch the agent into plan mode, so the brief
    /// arrives as its first planning request; empty sends none.
    pub plan_command: String,
    pub layout: WorkspaceLayout,
    /// Delete the review branch when its worktree is removed.
    pub delete_branch: bool,
    /// Other items that get the workspace too, e.g. the rest of a stack reviewed at once.
    /// It is removed on resolution only once every one of them has resolved.
    pub shared_with: Vec<String>,
}

/// Choice that brings the tracker up to date with work already started, e.g. assigning
/// the ticket and moving it to In Progress. Its action is `Perform`.
pub(crate) const START_WORK_CHOICE_ID: &str = "start_work";
/// Choice that stops the start reminder for one item. Handled by Herdr, not the source.
pub(crate) const MUTE_START_REMINDER_CHOICE_ID: &str = "mute_start_reminder";
/// Marks an item's linked draft pull request ready for review. Handled by Herdr through
/// the source hosting the pull request.
pub(crate) const PULL_REQUEST_READY_CHOICE_ID: &str = "pull_request_ready";
/// Opens an item's linked pull request in the browser.
pub(crate) const PULL_REQUEST_OPEN_CHOICE_ID: &str = "pull_request_open";
/// Moves an item's ticket to a done status once its linked pull request is merged. Its
/// action is `Perform`, carried out by the item's source.
pub(crate) const CLOSE_TICKET_CHOICE_ID: &str = "close_ticket";
/// Links an item's repository, mapped to no local clone, to its clone inside the source's
/// clone root, cloning it there first when there is none. Its action is `Perform`, carried
/// out by Herdr through the item's source with [`WorkItemSource::link_clone`].
pub(crate) const LINK_CLONE_CHOICE_ID: &str = "link_clone";
/// Marks a local item done, which ends any attention for it. Its action is `Perform`, like
/// [`MUTE_START_REMINDER_CHOICE_ID`], and Herdr carries it out itself.
pub(crate) const LOCAL_DONE_CHOICE_ID: &str = "done";
/// Takes a local item's `done` back. Handled like [`LOCAL_DONE_CHOICE_ID`].
pub(crate) const LOCAL_REOPEN_CHOICE_ID: &str = "reopen";

/// What a local item offers. No source knows it, so the only choice is Herdr's own: to mark it
/// done, or, once it is, to take that back. Neither is a default.
pub(crate) fn local_choices(resolved: bool) -> ItemChoices {
    let (choice_id, label) = if resolved {
        (LOCAL_REOPEN_CHOICE_ID, "Not done yet")
    } else {
        (LOCAL_DONE_CHOICE_ID, "Mark as done")
    };
    ItemChoices {
        choices: vec![WorkItemChoiceInfo {
            choice_id: choice_id.into(),
            label: label.into(),
            description: None,
            action: WorkItemChoiceAction::Perform,
            disabled_reason: None,
            confirm: None,
            options: Vec::new(),
            agent: None,
        }],
        default_choice_id: None,
    }
}

/// A local clone linked from an item at runtime. Kept in `work-items.json` and used by its
/// source like a clone mapped in the config, which wins for the same repository.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LinkedClone {
    pub source_id: String,
    /// The repository it is a clone of, e.g. "owner/name".
    pub name: String,
    pub path: PathBuf,
    /// Git remote that serves the repository.
    pub remote: String,
}

/// Starts the id of a choice carried onto a ticket from the pull request item folded into it:
/// the rest is the folded item's own id for it. Shared with clients through the API schema.
pub(crate) const PULL_REQUEST_CHOICE_PREFIX: &str =
    crate::api::schema::WORK_ITEM_PULL_REQUEST_CHOICE_PREFIX;

/// The id a ticket offers for the folded pull request item's choice `original_id`.
pub(crate) fn carried_choice_id(original_id: &str) -> String {
    format!("{PULL_REQUEST_CHOICE_PREFIX}{original_id}")
}

/// The folded item's own id for a choice a ticket carries, `None` for the ticket's own.
pub(crate) fn carried_original_id(choice_id: &str) -> Option<&str> {
    choice_id.strip_prefix(PULL_REQUEST_CHOICE_PREFIX)
}

/// The work landed, but the tracker still has the ticket open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloseTicket {
    /// One line for the user, e.g. "owner/repo#12 merged; TECH-7 is still In Progress".
    pub reason: String,
    /// The fix, whose id is `CLOSE_TICKET_CHOICE_ID`.
    pub choice: crate::api::schema::WorkItemChoiceInfo,
}

/// The tracker has not caught up with work you started: the ticket is not assigned to
/// you, or still in a to-do status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartReminder {
    /// One line for the item dialog, e.g. "You're working on this, but it isn't assigned
    /// to you and is still To Do".
    pub message: String,
    /// The one-step fix, whose id is `START_WORK_CHOICE_ID`; disabled with a reason when
    /// the source cannot carry it out.
    pub choice: crate::api::schema::WorkItemChoiceInfo,
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
    /// Pure: plan for `choice_id`, whose action provisions a workspace. `options` are the ids
    /// of the choice's options switched on.
    fn provision_plan(
        &self,
        item: &WorkItem,
        choice_id: &str,
        options: &[String],
        worktree_directory: &Path,
    ) -> Result<ProvisionPlan, String>;
    /// Pure: whether the item's workspace is removed once the source stops reporting it.
    fn remove_on_resolved(&self, item: &WorkItem) -> bool;
    /// Pure: notification text when an item arrives or is requested again.
    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>);
    /// Pure: the repository or tracker project the item with `external_id` belongs to, e.g.
    /// `owner/name` or `APP`, compared without regard to case. `None` when it belongs to none.
    fn project_of(&self, _external_id: &str) -> Option<String> {
        None
    }
    /// Blocking; background thread only. Searches the tracker with its own query syntax
    /// (JQL for Jira, GitHub search syntax for GitHub), read-only.
    fn search(&self, query: &str) -> Result<Vec<crate::api::schema::WorkItemTicketInfo>, String>;
    /// Blocking; background thread only. Fetches one ticket by its tracker key (e.g.
    /// `TECH-123` or `owner/repo#12`); `None` when the key does not exist.
    fn fetch(&self, key: &str) -> Result<Option<TicketDetail>, String>;
    /// Blocking; background thread only. Fetches an image a ticket's description or comment
    /// points at, with the source's credentials. Sources that fetch none leave this as is.
    fn image(&self, _url: &str) -> Result<TicketImage, String> {
        Ok(TicketImage::UnsupportedUrl)
    }
    /// Pure: plan for this provider's shared "Pick next" discovery workspace (a scratch
    /// directory; no checkout) running `agent`, briefed with `context` (may be empty) to
    /// investigate the tracker read-only and recommend what to work on next.
    fn pick_next_plan(
        &self,
        context: &str,
        worktree_directory: &Path,
        agent: &crate::config::AgentLaunch,
    ) -> Result<ProvisionPlan, String>;
    /// Blocking; background thread only. Carries out a choice whose action is `Perform`
    /// and returns a one-line result for the user.
    fn perform(&self, _item: &WorkItem, choice_id: &str) -> Result<String, String> {
        Err(format!("choice {choice_id} cannot be carried out here"))
    }
    /// Blocking; background thread only. The clone of `item`'s repository inside the clone
    /// root, cloned there first when there is none, for [`LINK_CLONE_CHOICE_ID`]; with a
    /// one-line result for the user.
    fn link_clone(&self, _item: &WorkItem) -> Result<(LinkedClone, String), String> {
        Err("this source cannot link clones".into())
    }
    /// Pure: whether the tracker lags behind work started on `item`. Only asked for items
    /// with a workspace whose reminder is not muted.
    fn start_reminder(&self, _item: &WorkItem) -> Option<StartReminder> {
        None
    }
    /// Pure: how to close `item`'s ticket now that `pull_request`, found for its workspace,
    /// is merged. `None` when the ticket is already done or the source cannot close it.
    fn close_ticket(
        &self,
        _item: &WorkItem,
        _pull_request: &crate::api::schema::WorkItemPullRequestInfo,
    ) -> Option<CloseTicket> {
        None
    }
    /// Pure: the local branch `item`'s work is on, as (clone, branch), so its pull request
    /// can still be found once the item has no workspace. `None` when unknown.
    fn work_branch(&self, _item: &WorkItem) -> Option<(std::path::PathBuf, String)> {
        None
    }
    /// Pure: text sent to the agent in the item's workspace for a choice whose action is
    /// `BriefAgent`.
    fn follow_up_brief(&self, _item: &WorkItem, choice_id: &str) -> Result<String, String> {
        Err(format!("choice {choice_id} does not brief an agent"))
    }
    /// Blocking; background thread only. The pull request opened from `branch` of the
    /// clone at `repo_root`, the most recent one if several, when this source hosts it.
    fn find_pull_request(
        &self,
        _repo_root: &Path,
        _branch: &str,
    ) -> Result<Option<crate::api::schema::WorkItemPullRequestInfo>, String> {
        Ok(None)
    }
    /// Blocking; background thread only. Marks a draft pull request found by
    /// `find_pull_request` ready for review.
    fn mark_pull_request_ready(
        &self,
        pull_request: &crate::api::schema::WorkItemPullRequestInfo,
    ) -> Result<String, String> {
        Err(format!(
            "{}#{} cannot be changed here",
            pull_request.repo, pull_request.number
        ))
    }
    /// Pure: the pull request (repository, number) an item of this source is, if any.
    fn pull_request_of(&self, _item: &WorkItem) -> Option<(String, u64)> {
        None
    }
    /// Blocking; background thread only. Where each of `pulls`, (repository, number) pairs
    /// from `pull_request_of`, stands now, in order; `None` for one that could not be read.
    fn pull_request_statuses(
        &self,
        pulls: &[(String, u64)],
    ) -> Result<Vec<Option<crate::api::schema::WorkItemPullRequestInfo>>, String> {
        Ok(vec![None; pulls.len()])
    }
    /// Pure: a choice working on the whole stack `item`'s own pull request is part of, when
    /// offered. `members` are the other items of this source whose own pull requests are in
    /// that stack. Its plan lists in `ProvisionPlan::shared_with` the members that share the
    /// workspace.
    fn stack_choice(
        &self,
        _item: &WorkItem,
        _members: &[&WorkItem],
    ) -> Option<crate::api::schema::WorkItemChoiceInfo> {
        None
    }
    /// Pure: the key of one of this source's tickets that `title`, the title of another
    /// source's item, starts with, e.g. `TECH-12` for "[TECH-12] Fix login".
    fn ticket_key_in_title(&self, _title: &str) -> Option<String> {
        None
    }
    /// Blocking; background thread only. The status line of the ticket named `key`, for
    /// the items whose titles name it; `None` when the key does not exist.
    fn linked_ticket(
        &self,
        _key: &str,
    ) -> Result<Option<crate::api::schema::WorkItemLinkedTicketInfo>, String> {
        Ok(None)
    }
    /// Pure: what the tracker asks of you for `item`, whatever Herdr is doing about it.
    /// Newly arrived by default; Herdr only reports `New` until a choice is made.
    fn tracker_need(&self, item: &WorkItem) -> Option<super::attention::Need> {
        let (title, _) = self.arrival_notice(&item.source_item());
        Some(super::attention::Need::new(
            crate::api::schema::AttentionKind::New,
            title,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceOptionInfo};

    #[test]
    fn ticket_image_bytes_are_typed_by_their_first_bytes_and_capped() {
        let typed = |bytes: &[u8]| match TicketImage::from_bytes(bytes.to_vec()) {
            TicketImage::Available { media_type, .. } => Some(media_type),
            _ => None,
        };
        assert_eq!(typed(b"\x89PNG\r\n\x1a\n..."), Some("image/png"));
        assert_eq!(typed(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(typed(b"GIF89a..."), Some("image/gif"));
        assert_eq!(typed(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(
            TicketImage::from_bytes(b"<!DOCTYPE html>".to_vec()),
            TicketImage::NotAnImage
        );
        let mut huge = b"\x89PNG\r\n\x1a\n".to_vec();
        huge.resize(MAX_TICKET_IMAGE_BYTES + 1, 0);
        assert_eq!(
            TicketImage::from_bytes(huge),
            TicketImage::TooLarge {
                byte_count: MAX_TICKET_IMAGE_BYTES as u64 + 1
            }
        );
    }

    fn choice_with(options: &[(&str, bool)]) -> WorkItemChoiceInfo {
        WorkItemChoiceInfo {
            choice_id: "review".into(),
            label: "Review".into(),
            description: None,
            action: WorkItemChoiceAction::ProvisionWorkspace,
            disabled_reason: None,
            confirm: None,
            options: options
                .iter()
                .map(|(id, default)| WorkItemChoiceOptionInfo {
                    option_id: (*id).into(),
                    label: (*id).into(),
                    description: None,
                    default: *default,
                })
                .collect(),
            agent: None,
        }
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn naming_no_options_switches_on_the_ones_on_by_default() {
        let choice = choice_with(&[("worktree", true), ("post", false)]);
        assert_eq!(switched_on_options(&choice, None), Ok(ids(&["worktree"])));
    }

    #[test]
    fn naming_options_switches_on_exactly_those_in_the_choices_order() {
        let choice = choice_with(&[("worktree", true), ("post", false)]);
        assert_eq!(
            switched_on_options(&choice, Some(&ids(&["post", "worktree", "post"]))),
            Ok(ids(&["worktree", "post"]))
        );
        assert_eq!(
            switched_on_options(&choice, Some(&ids(&["post"]))),
            Ok(ids(&["post"]))
        );
        // An empty list switches every option off; it is not the defaults.
        assert_eq!(switched_on_options(&choice, Some(&[])), Ok(Vec::new()));
    }

    #[test]
    fn an_option_the_choice_does_not_offer_is_rejected_next_to_valid_ones_too() {
        let choice = choice_with(&[("worktree", true)]);
        assert_eq!(
            switched_on_options(&choice, Some(&ids(&["worktree", "teleport"]))),
            Err("teleport")
        );

        let plain = choice_with(&[]);
        assert_eq!(
            switched_on_options(&plain, Some(&ids(&["worktree"]))),
            Err("worktree")
        );
    }
}
