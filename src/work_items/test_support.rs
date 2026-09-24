//! Test-only scripted work-item source.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::source::ItemChoices;
use super::state::WorkItem;
use super::{PreparedItem, ProvisionPlan, SourceItem, WorkItemSource};
use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo};

/// Scripted source: each poll returns the current `items`.
#[derive(Default)]
pub(crate) struct FakeSource {
    pub items: Mutex<Vec<SourceItem>>,
    pub prepare_calls: AtomicUsize,
    /// Plan returned for the "local" choice; `None` makes it unavailable.
    pub plan: Mutex<Option<ProvisionPlan>>,
}

impl FakeSource {
    pub(crate) fn with_items(items: Vec<SourceItem>) -> Arc<Self> {
        Arc::new(Self {
            items: Mutex::new(items),
            prepare_calls: AtomicUsize::new(0),
            plan: Mutex::new(None),
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
            detail: None,
            summary: Some("+1 −0 across 1 file".into()),
            error: None,
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
                },
                WorkItemChoiceInfo {
                    choice_id: "web".into(),
                    label: "Open".into(),
                    description: None,
                    action: WorkItemChoiceAction::OpenUrl {
                        url: item.url.clone(),
                    },
                    disabled_reason: None,
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

    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>) {
        ("Arrived".into(), Some(item.title.clone()))
    }
}
