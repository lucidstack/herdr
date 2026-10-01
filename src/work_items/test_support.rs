//! Test-only scripted work-item source.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::source::{ItemChoices, TicketDetail};
use super::state::WorkItem;
use super::{PreparedItem, ProvisionPlan, SourceItem, WorkItemSource};
use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo, WorkItemTicketInfo};

/// Scripted source: each poll returns the current `items`.
#[derive(Default)]
pub(crate) struct FakeSource {
    id: &'static str,
    /// Titles whose first word starts with this prefix name that word as one of its tickets.
    ticket_prefix: Option<&'static str>,
    pub items: Mutex<Vec<SourceItem>>,
    pub prepare_calls: AtomicUsize,
    /// Plan returned for the "local" choice; `None` makes it unavailable.
    pub plan: Mutex<Option<ProvisionPlan>>,
    /// Whether resolved items have their workspace removed.
    pub remove_on_resolved: std::sync::atomic::AtomicBool,
    /// Result of the "do" choice; `None` makes it succeed.
    pub perform_error: Mutex<Option<String>>,
    /// Returned by `search`, regardless of the query.
    pub search_results: Mutex<Vec<WorkItemTicketInfo>>,
    /// Makes `search` fail with this message instead.
    pub search_error: Mutex<Option<String>>,
    /// Returned by `fetch`, regardless of the key; `None` means "not found".
    pub fetch_ticket: Mutex<Option<TicketDetail>>,
    /// Makes `fetch` fail with this message instead.
    pub fetch_error: Mutex<Option<String>>,
    /// Plan returned by `pick_next_plan`; `None` makes it unavailable.
    pub scripted_pick_next_plan: Mutex<Option<ProvisionPlan>>,
    /// Every `context` passed to `pick_next_plan`, in call order.
    pub pick_next_contexts: Mutex<Vec<String>>,
    /// Reminder line for every item asked about; `None` means the tracker is up to date.
    pub start_reminder: Mutex<Option<String>>,
    /// Returned by `find_pull_request` for any branch.
    pub pull_request: Mutex<Option<crate::api::schema::WorkItemPullRequestInfo>>,
    /// Every (repository, number) marked ready for review, in call order.
    pub marked_ready: Mutex<Vec<(String, u64)>>,
    /// Items whose external id starts with this prefix are pull request `o/r#<id>`.
    pub pull_request_prefix: Mutex<Option<String>>,
    /// Offers to close an item's ticket once its linked pull request is merged.
    pub closes_merged_tickets: std::sync::atomic::AtomicBool,
    /// Branch the work is on while an item has no workspace; `None` means unknown.
    pub work_branch: Mutex<Option<String>>,
    /// Every branch `find_pull_request` was asked about, in call order.
    pub looked_up_branches: Mutex<Vec<String>>,
}

impl FakeSource {
    pub(crate) fn with_items(items: Vec<SourceItem>) -> Arc<Self> {
        Arc::new(Self::new("fake", None, items))
    }

    /// A second source, `id`, whose tickets are named by titles starting with `prefix`.
    pub(crate) fn tracker(id: &'static str, prefix: &'static str) -> Arc<Self> {
        Arc::new(Self::new(id, Some(prefix), Vec::new()))
    }

    fn new(id: &'static str, ticket_prefix: Option<&'static str>, items: Vec<SourceItem>) -> Self {
        Self {
            id,
            ticket_prefix,
            items: Mutex::new(items),
            prepare_calls: AtomicUsize::new(0),
            plan: Mutex::new(None),
            remove_on_resolved: std::sync::atomic::AtomicBool::new(false),
            perform_error: Mutex::new(None),
            search_results: Mutex::new(Vec::new()),
            search_error: Mutex::new(None),
            fetch_ticket: Mutex::new(None),
            fetch_error: Mutex::new(None),
            scripted_pick_next_plan: Mutex::new(None),
            pick_next_contexts: Mutex::new(Vec::new()),
            start_reminder: Mutex::new(None),
            pull_request: Mutex::new(None),
            marked_ready: Mutex::new(Vec::new()),
            pull_request_prefix: Mutex::new(None),
            closes_merged_tickets: std::sync::atomic::AtomicBool::new(false),
            work_branch: Mutex::new(None),
            looked_up_branches: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn set_items(&self, items: Vec<SourceItem>) {
        *self.items.lock().expect("fake source lock") = items;
    }

    pub(crate) fn prepare_calls(&self) -> usize {
        self.prepare_calls.load(Ordering::SeqCst)
    }
}

pub(crate) fn source_item(id: &str) -> SourceItem {
    SourceItem {
        external_id: id.to_string(),
        title: format!("Title {id}"),
        context: format!("repo {id}"),
        author: Some("octocat".into()),
        url: format!("https://example.test/{id}"),
        updated_at: "2026-01-01T00:00:00Z".into(),
        tracker_state: None,
    }
}

impl WorkItemSource for FakeSource {
    fn id(&self) -> &str {
        self.id
    }

    fn label(&self) -> &str {
        "Fake"
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn poll(&self) -> Result<Vec<SourceItem>, String> {
        Ok(self.items.lock().expect("fake source lock").clone())
    }

    fn ticket_key_in_title(&self, title: &str) -> Option<String> {
        let word = title.split_whitespace().next()?;
        word.starts_with(self.ticket_prefix?)
            .then(|| word.to_string())
    }

    fn start_reminder(&self, _item: &WorkItem) -> Option<super::source::StartReminder> {
        let message = self
            .start_reminder
            .lock()
            .expect("fake source lock")
            .clone()?;
        Some(super::source::StartReminder {
            message,
            choice: WorkItemChoiceInfo {
                choice_id: super::source::START_WORK_CHOICE_ID.into(),
                label: "Assign to me".into(),
                description: None,
                action: WorkItemChoiceAction::Perform,
                disabled_reason: None,
                confirm: None,
            },
        })
    }

    fn close_ticket(
        &self,
        item: &WorkItem,
        pull_request: &crate::api::schema::WorkItemPullRequestInfo,
    ) -> Option<super::source::CloseTicket> {
        self.closes_merged_tickets
            .load(Ordering::SeqCst)
            .then(|| super::source::CloseTicket {
                reason: format!(
                    "#{} merged; {} is still open",
                    pull_request.number, item.key
                ),
                choice: WorkItemChoiceInfo {
                    choice_id: super::source::CLOSE_TICKET_CHOICE_ID.into(),
                    label: "Move to Done".into(),
                    description: None,
                    action: WorkItemChoiceAction::Perform,
                    disabled_reason: None,
                    confirm: None,
                },
            })
    }

    fn prepare(&self, _item: &SourceItem) -> PreparedItem {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        PreparedItem {
            waiting: false,
            detail: None,
            summary: Some("+1 −0 across 1 file".into()),
            error: None,
            done: false,
        }
    }

    fn choices(&self, item: &WorkItem) -> ItemChoices {
        ItemChoices {
            choices: vec![
                WorkItemChoiceInfo {
                    choice_id: "local".into(),
                    label: "Local".into(),
                    description: None,
                    action: WorkItemChoiceAction::ProvisionWorkspace,
                    disabled_reason: None,
                    confirm: None,
                },
                WorkItemChoiceInfo {
                    choice_id: "web".into(),
                    label: "Open".into(),
                    description: None,
                    action: WorkItemChoiceAction::OpenUrl {
                        url: item.url.clone(),
                    },
                    disabled_reason: None,
                    confirm: None,
                },
                WorkItemChoiceInfo {
                    choice_id: "do".into(),
                    label: "Do it".into(),
                    description: None,
                    action: WorkItemChoiceAction::Perform,
                    disabled_reason: None,
                    confirm: Some("Sure?".into()),
                },
                WorkItemChoiceInfo {
                    choice_id: "brief".into(),
                    label: "Brief the agent".into(),
                    description: None,
                    action: WorkItemChoiceAction::BriefAgent,
                    disabled_reason: None,
                    confirm: None,
                },
            ],
            default_choice_id: Some("web".into()),
        }
    }

    fn provision_plan(
        &self,
        _item: &WorkItem,
        _choice_id: &str,
        _worktree_directory: &std::path::Path,
    ) -> Result<ProvisionPlan, String> {
        self.plan
            .lock()
            .expect("fake source lock")
            .clone()
            .ok_or_else(|| "no plan scripted".to_string())
    }

    fn remove_on_resolved(&self, _item: &WorkItem) -> bool {
        self.remove_on_resolved.load(Ordering::SeqCst)
    }

    fn perform(&self, _item: &WorkItem, _choice_id: &str) -> Result<String, String> {
        match self.perform_error.lock().expect("fake source lock").clone() {
            Some(error) => Err(error),
            None => Ok("Done".into()),
        }
    }

    fn follow_up_brief(&self, _item: &WorkItem, _choice_id: &str) -> Result<String, String> {
        Ok("hello".into())
    }

    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>) {
        ("Arrived".into(), Some(item.title.clone()))
    }

    fn search(&self, _query: &str) -> Result<Vec<WorkItemTicketInfo>, String> {
        match self.search_error.lock().expect("fake source lock").clone() {
            Some(error) => Err(error),
            None => Ok(self
                .search_results
                .lock()
                .expect("fake source lock")
                .clone()),
        }
    }

    fn fetch(&self, _key: &str) -> Result<Option<TicketDetail>, String> {
        match self.fetch_error.lock().expect("fake source lock").clone() {
            Some(error) => Err(error),
            None => Ok(self.fetch_ticket.lock().expect("fake source lock").clone()),
        }
    }

    fn pick_next_plan(
        &self,
        context: &str,
        _worktree_directory: &std::path::Path,
    ) -> Result<ProvisionPlan, String> {
        self.pick_next_contexts
            .lock()
            .expect("fake source lock")
            .push(context.to_string());
        self.scripted_pick_next_plan
            .lock()
            .expect("fake source lock")
            .clone()
            .ok_or_else(|| "no pick-next plan scripted".to_string())
    }

    fn find_pull_request(
        &self,
        _repo_root: &std::path::Path,
        branch: &str,
    ) -> Result<Option<crate::api::schema::WorkItemPullRequestInfo>, String> {
        self.looked_up_branches
            .lock()
            .expect("fake source lock")
            .push(branch.to_string());
        Ok(self.pull_request.lock().expect("fake source lock").clone())
    }

    fn work_branch(&self, _item: &WorkItem) -> Option<(std::path::PathBuf, String)> {
        let branch = self.work_branch.lock().expect("fake source lock").clone()?;
        Some(("/src/app".into(), branch))
    }

    fn mark_pull_request_ready(
        &self,
        pull_request: &crate::api::schema::WorkItemPullRequestInfo,
    ) -> Result<String, String> {
        self.marked_ready
            .lock()
            .expect("fake source lock")
            .push((pull_request.repo.clone(), pull_request.number));
        Ok(format!("#{} is ready for review", pull_request.number))
    }

    fn pull_request_of(&self, item: &WorkItem) -> Option<(String, u64)> {
        let prefix = self
            .pull_request_prefix
            .lock()
            .expect("fake source lock")
            .clone()?;
        let number = item.external_id.strip_prefix(&prefix)?.parse().ok()?;
        Some(("o/r".into(), number))
    }
}
