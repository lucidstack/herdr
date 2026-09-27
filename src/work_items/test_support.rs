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
}

impl FakeSource {
    pub(crate) fn with_items(items: Vec<SourceItem>) -> Arc<Self> {
        Arc::new(Self {
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
        })
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
    }
}

impl WorkItemSource for FakeSource {
    fn id(&self) -> &str {
        "fake"
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
}
