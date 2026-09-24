//! Pure work-item state and transitions. No I/O.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::api::schema::{WorkItemInfo, WorkItemPhase, WorkItemProvisioningInfo};

use super::source::{ItemChoices, PreparedItem, SourceItem};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkItem {
    pub key: String,
    pub source_id: String,
    pub external_id: String,
    pub title: String,
    pub context: String,
    pub author: Option<String>,
    pub url: String,
    pub updated_at: String,
    #[serde(default)]
    pub detail: Option<serde_json::Value>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub prepare_error: Option<String>,
    pub phase: WorkItemPhase,
    pub seen: bool,
    #[serde(default)]
    pub resolved: bool,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Hidden until the source stops listing the item or requests it again.
    #[serde(default)]
    pub dismissed: bool,
    /// Hidden until this Unix time (seconds); cleared once it passes.
    #[serde(default)]
    pub snoozed_until: Option<u64>,
    /// `updated_at` the prepared detail belongs to.
    #[serde(skip)]
    pub prepared_for: Option<String>,
    #[serde(skip)]
    pub prepare_in_flight: bool,
    #[serde(skip)]
    pub provisioning: Option<WorkItemProvisioningInfo>,
    /// Why removing the workspace on resolution failed.
    #[serde(skip)]
    pub resolve_error: Option<String>,
}

pub(crate) fn item_key(source_id: &str, external_id: &str) -> String {
    format!("{source_id}:{external_id}")
}

impl WorkItem {
    fn new(source_id: &str, item: SourceItem) -> Self {
        Self {
            key: item_key(source_id, &item.external_id),
            source_id: source_id.to_string(),
            external_id: item.external_id,
            title: item.title,
            context: item.context,
            author: item.author,
            url: item.url,
            updated_at: item.updated_at,
            detail: None,
            summary: None,
            prepare_error: None,
            phase: WorkItemPhase::Pending,
            seen: false,
            resolved: false,
            workspace_id: None,
            dismissed: false,
            snoozed_until: None,
            prepared_for: None,
            prepare_in_flight: false,
            provisioning: None,
            resolve_error: None,
        }
    }

    pub(crate) fn source_item(&self) -> SourceItem {
        SourceItem {
            external_id: self.external_id.clone(),
            title: self.title.clone(),
            context: self.context.clone(),
            author: self.author.clone(),
            url: self.url.clone(),
            updated_at: self.updated_at.clone(),
        }
    }

    pub(crate) fn info(&self, choices: ItemChoices) -> WorkItemInfo {
        WorkItemInfo {
            item_id: self.key.clone(),
            source_id: self.source_id.clone(),
            context: self.context.clone(),
            title: self.title.clone(),
            author: self.author.clone(),
            url: self.url.clone(),
            summary: self.summary.clone(),
            notice: self
                .resolve_error
                .clone()
                .or_else(|| self.prepare_error.clone()),
            phase: self.phase,
            seen: self.seen,
            resolved: self.resolved,
            workspace_id: self.workspace_id.clone(),
            dismissed: self.dismissed,
            snoozed_until: self.snoozed_until,
            choices: choices.choices,
            default_choice_id: choices.default_choice_id,
            provisioning: self.provisioning.clone(),
        }
    }

    fn unlink_workspace(&mut self) {
        self.workspace_id = None;
        self.provisioning = None;
        self.phase = WorkItemPhase::Pending;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotFound;

#[derive(Debug, Default)]
pub(crate) struct WorkItemsState {
    items: Vec<WorkItem>,
    source_errors: HashMap<String, String>,
    /// Keys that became resolved since the last `take_newly_resolved`.
    newly_resolved: Vec<String>,
}

impl WorkItemsState {
    pub(crate) fn from_items(items: Vec<WorkItem>) -> Self {
        Self {
            items,
            source_errors: HashMap::new(),
            newly_resolved: Vec::new(),
        }
    }

    pub(crate) fn items(&self) -> &[WorkItem] {
        &self.items
    }

    pub(crate) fn get(&self, key: &str) -> Option<&WorkItem> {
        self.items.iter().find(|item| item.key == key)
    }

    pub(crate) fn get_mut(&mut self, key: &str) -> Option<&mut WorkItem> {
        self.items.iter_mut().find(|item| item.key == key)
    }

    pub(crate) fn source_error(&self, source_id: &str) -> Option<&str> {
        self.source_errors.get(source_id).map(String::as_str)
    }

    /// Applies one poll result. Returns whether anything changed and the keys of
    /// items that newly arrived or were requested again.
    pub(crate) fn apply_poll(
        &mut self,
        source_id: &str,
        result: Result<Vec<SourceItem>, String>,
    ) -> (bool, Vec<String>) {
        let polled = match result {
            Ok(polled) => polled,
            Err(message) => {
                let changed = self.source_errors.get(source_id) != Some(&message);
                self.source_errors.insert(source_id.to_string(), message);
                return (changed, Vec::new());
            }
        };
        let mut changed = self.source_errors.remove(source_id).is_some();
        let mut arrivals = Vec::new();
        let mut present = HashSet::new();

        for source_item in polled {
            let key = item_key(source_id, &source_item.external_id);
            present.insert(key.clone());
            let Some(item) = self.items.iter_mut().find(|item| item.key == key) else {
                self.items.push(WorkItem::new(source_id, source_item));
                arrivals.push(key);
                changed = true;
                continue;
            };
            let before = item.clone();
            item.title = source_item.title;
            item.context = source_item.context;
            item.author = source_item.author;
            item.url = source_item.url;
            item.updated_at = source_item.updated_at;
            if item.resolved {
                item.resolved = false;
                item.seen = false;
                // A new request brings a hidden item back.
                item.dismissed = false;
                item.snoozed_until = None;
                arrivals.push(key);
            }
            if item.workspace_id.is_none() && item.phase == WorkItemPhase::Local {
                item.phase = WorkItemPhase::Pending;
            }
            changed |= *item != before;
        }

        let newly_resolved = &mut self.newly_resolved;
        self.items.retain_mut(|item| {
            if item.source_id != source_id || present.contains(&item.key) {
                return true;
            }
            if item.workspace_id.is_some() {
                if !item.resolved {
                    item.resolved = true;
                    newly_resolved.push(item.key.clone());
                    changed = true;
                }
                true
            } else {
                changed = true;
                false
            }
        });

        (changed, arrivals)
    }

    /// Items that became resolved while a workspace hangs off them.
    pub(crate) fn take_newly_resolved(&mut self) -> Vec<String> {
        std::mem::take(&mut self.newly_resolved)
    }

    /// Items of a source whose preparation is missing or stale; marks them in flight.
    pub(crate) fn needs_prepare(&mut self, source_id: &str) -> Vec<SourceItem> {
        self.items
            .iter_mut()
            .filter(|item| {
                item.source_id == source_id
                    && !item.prepare_in_flight
                    && item.prepared_for.as_deref() != Some(item.updated_at.as_str())
            })
            .map(|item| {
                item.prepare_in_flight = true;
                item.source_item()
            })
            .collect()
    }

    pub(crate) fn apply_prepared(
        &mut self,
        key: &str,
        updated_at: &str,
        prepared: PreparedItem,
    ) -> bool {
        let Some(item) = self.get_mut(key) else {
            return false;
        };
        let before = item.clone();
        item.detail = prepared.detail;
        item.summary = prepared.summary;
        item.prepare_error = prepared.error;
        item.prepared_for = Some(updated_at.to_string());
        item.prepare_in_flight = false;
        *item != before
    }

    pub(crate) fn mark_seen(&mut self, key: &str) -> Result<bool, NotFound> {
        let item = self.get_mut(key).ok_or(NotFound)?;
        let changed = !item.seen;
        item.seen = true;
        Ok(changed)
    }

    pub(crate) fn set_awaiting_external(&mut self, key: &str) -> Result<bool, NotFound> {
        let item = self.get_mut(key).ok_or(NotFound)?;
        let changed = item.phase != WorkItemPhase::AwaitingExternal || !item.seen;
        item.phase = WorkItemPhase::AwaitingExternal;
        item.seen = true;
        Ok(changed)
    }

    /// Hides an item: dismissed when `snooze_until` is `None`, else snoozed until then.
    pub(crate) fn hide(&mut self, key: &str, snooze_until: Option<u64>) -> Result<bool, NotFound> {
        let item = self.get_mut(key).ok_or(NotFound)?;
        let before = (item.dismissed, item.snoozed_until, item.seen);
        item.dismissed = snooze_until.is_none();
        item.snoozed_until = snooze_until;
        item.seen = true;
        Ok(before != (item.dismissed, item.snoozed_until, item.seen))
    }

    /// Brings a dismissed or snoozed item back.
    pub(crate) fn unhide(&mut self, key: &str) -> Result<bool, NotFound> {
        let item = self.get_mut(key).ok_or(NotFound)?;
        let changed = item.dismissed || item.snoozed_until.is_some();
        item.dismissed = false;
        item.snoozed_until = None;
        Ok(changed)
    }

    /// Ends snoozes that passed `now` (Unix seconds); woken items count as new again.
    pub(crate) fn expire_snoozes(&mut self, now: u64) -> bool {
        let mut changed = false;
        for item in &mut self.items {
            if item.snoozed_until.is_some_and(|until| until <= now) {
                item.snoozed_until = None;
                item.seen = false;
                changed = true;
            }
        }
        changed
    }

    /// The earliest snooze end, in Unix seconds.
    pub(crate) fn next_snooze_end(&self) -> Option<u64> {
        self.items
            .iter()
            .filter_map(|item| item.snoozed_until)
            .min()
    }

    pub(crate) fn workspace_closed(&mut self, workspace_id: &str) -> bool {
        let Some(index) = self
            .items
            .iter()
            .position(|item| item.workspace_id.as_deref() == Some(workspace_id))
        else {
            return false;
        };
        if self.items[index].resolved {
            self.items.remove(index);
        } else {
            self.items[index].unlink_workspace();
        }
        true
    }

    pub(crate) fn reconcile_workspaces(&mut self, existing: &HashSet<&str>) -> bool {
        let missing: Vec<String> = self
            .items
            .iter()
            .filter_map(|item| item.workspace_id.clone())
            .filter(|id| !existing.contains(id.as_str()))
            .collect();
        let mut changed = false;
        for id in missing {
            changed |= self.workspace_closed(&id);
        }
        changed
    }

    /// Drops items of unconfigured sources unless a workspace hangs off them.
    pub(crate) fn retain_sources(&mut self, source_ids: &HashSet<&str>) -> bool {
        let before = self.items.len();
        self.items.retain(|item| {
            source_ids.contains(item.source_id.as_str()) || item.workspace_id.is_some()
        });
        self.source_errors
            .retain(|source_id, _| source_ids.contains(source_id.as_str()));
        before != self.items.len()
    }

    /// Unseen first, then pending, awaiting, local; newest first within a group.
    pub(crate) fn sorted(&self) -> Vec<&WorkItem> {
        let mut items: Vec<&WorkItem> = self.items.iter().collect();
        items.sort_by(|a, b| {
            a.seen
                .cmp(&b.seen)
                .then_with(|| phase_rank(a.phase).cmp(&phase_rank(b.phase)))
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        items
    }
}

fn phase_rank(phase: WorkItemPhase) -> u8 {
    match phase {
        WorkItemPhase::Pending => 0,
        WorkItemPhase::AwaitingExternal => 1,
        WorkItemPhase::Local => 2,
        WorkItemPhase::Unknown => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polled(id: &str, updated_at: &str) -> SourceItem {
        SourceItem {
            external_id: id.to_string(),
            title: format!("title {id}"),
            context: format!("context {id}"),
            author: Some("octocat".into()),
            url: format!("https://example.test/{id}"),
            updated_at: updated_at.to_string(),
        }
    }

    fn state_with(source: &str, ids: &[&str]) -> WorkItemsState {
        let mut state = WorkItemsState::default();
        state.apply_poll(
            source,
            Ok(ids
                .iter()
                .map(|id| polled(id, "2026-01-01T00:00:00Z"))
                .collect()),
        );
        state
    }

    #[test]
    fn new_item_is_unseen_pending_and_reported_as_arrival() {
        let mut state = WorkItemsState::default();
        let (changed, arrivals) = state.apply_poll("gh", Ok(vec![polled("a", "t1")]));
        assert!(changed);
        assert_eq!(arrivals, vec!["gh:a".to_string()]);
        let item = state.get("gh:a").expect("item stored");
        assert!(!item.seen);
        assert_eq!(item.phase, WorkItemPhase::Pending);
    }

    #[test]
    fn unchanged_repoll_reports_no_change() {
        let mut state = state_with("gh", &["a"]);
        let (changed, arrivals) =
            state.apply_poll("gh", Ok(vec![polled("a", "2026-01-01T00:00:00Z")]));
        assert!(!changed);
        assert!(arrivals.is_empty());
    }

    #[test]
    fn missing_item_without_workspace_is_removed() {
        let mut state = state_with("gh", &["a"]);
        state.set_awaiting_external("gh:a").expect("item exists");
        let (changed, _) = state.apply_poll("gh", Ok(Vec::new()));
        assert!(changed);
        assert!(state.get("gh:a").is_none());
    }

    #[test]
    fn missing_item_with_workspace_becomes_resolved_and_is_kept() {
        let mut state = state_with("gh", &["a"]);
        state.get_mut("gh:a").expect("item").workspace_id = Some("w1".into());
        state.apply_poll("gh", Ok(Vec::new()));
        assert!(state.get("gh:a").expect("item kept").resolved);
    }

    #[test]
    fn resolution_of_an_item_with_workspace_is_reported_once() {
        let mut state = state_with("gh", &["a"]);
        state.get_mut("gh:a").expect("item").workspace_id = Some("w1".into());
        state.apply_poll("gh", Ok(Vec::new()));
        state.apply_poll("gh", Ok(Vec::new()));
        assert_eq!(state.take_newly_resolved(), vec!["gh:a".to_string()]);
        assert!(state.take_newly_resolved().is_empty());
    }

    #[test]
    fn resolved_item_reappearing_is_unresolved_unseen_and_an_arrival() {
        let mut state = state_with("gh", &["a"]);
        state.get_mut("gh:a").expect("item").workspace_id = Some("w1".into());
        state.mark_seen("gh:a").expect("item");
        state.apply_poll("gh", Ok(Vec::new()));
        let (_, arrivals) = state.apply_poll("gh", Ok(vec![polled("a", "t2")]));
        assert_eq!(arrivals, vec!["gh:a".to_string()]);
        let item = state.get("gh:a").expect("item");
        assert!(!item.resolved);
        assert!(!item.seen);
    }

    #[test]
    fn dismissed_item_stays_hidden_while_the_source_keeps_listing_it() {
        let mut state = state_with("gh", &["a"]);
        assert_eq!(state.hide("gh:a", None), Ok(true));
        state.apply_poll("gh", Ok(vec![polled("a", "t2")]));
        let item = state.get("gh:a").expect("item");
        assert!(item.dismissed);
        assert!(item.seen);
    }

    #[test]
    fn a_new_request_brings_a_dismissed_item_back() {
        let mut state = state_with("gh", &["a"]);
        state.get_mut("gh:a").expect("item").workspace_id = Some("w1".into());
        state.hide("gh:a", Some(500)).expect("item");
        state.apply_poll("gh", Ok(Vec::new()));
        state.apply_poll("gh", Ok(vec![polled("a", "t2")]));
        let item = state.get("gh:a").expect("item");
        assert!(!item.dismissed);
        assert_eq!(item.snoozed_until, None);
    }

    #[test]
    fn snooze_ends_at_its_time_and_the_item_counts_as_new() {
        let mut state = state_with("gh", &["a"]);
        state.hide("gh:a", Some(100)).expect("item");
        assert_eq!(state.next_snooze_end(), Some(100));
        assert!(!state.expire_snoozes(99));
        assert!(state.expire_snoozes(100));
        let item = state.get("gh:a").expect("item");
        assert_eq!(item.snoozed_until, None);
        assert!(!item.seen);
    }

    #[test]
    fn unhide_shows_an_item_again() {
        let mut state = state_with("gh", &["a"]);
        state.hide("gh:a", None).expect("item");
        assert_eq!(state.unhide("gh:a"), Ok(true));
        assert!(!state.get("gh:a").expect("item").dismissed);
        assert_eq!(state.unhide("gh:missing"), Err(NotFound));
    }

    #[test]
    fn failed_poll_keeps_items_and_records_error_until_next_success() {
        let mut state = state_with("gh", &["a"]);
        let (changed, _) = state.apply_poll("gh", Err("offline".into()));
        assert!(changed);
        assert!(state.get("gh:a").is_some());
        assert_eq!(state.source_error("gh"), Some("offline"));
        state.apply_poll("gh", Ok(vec![polled("a", "2026-01-01T00:00:00Z")]));
        assert_eq!(state.source_error("gh"), None);
    }

    #[test]
    fn poll_of_one_source_leaves_other_sources_items() {
        let mut state = state_with("a", &["x"]);
        state.apply_poll("b", Ok(Vec::new()));
        assert!(state.get("a:x").is_some());
    }

    #[test]
    fn closing_workspace_removes_resolved_item() {
        let mut state = state_with("gh", &["a"]);
        state.get_mut("gh:a").expect("item").workspace_id = Some("w1".into());
        state.apply_poll("gh", Ok(Vec::new()));
        assert!(state.workspace_closed("w1"));
        assert!(state.get("gh:a").is_none());
    }

    #[test]
    fn closing_workspace_returns_unresolved_item_to_pending() {
        let mut state = state_with("gh", &["a"]);
        let item = state.get_mut("gh:a").expect("item");
        item.workspace_id = Some("w1".into());
        item.phase = WorkItemPhase::Local;
        assert!(state.workspace_closed("w1"));
        let item = state.get("gh:a").expect("item kept");
        assert_eq!(item.phase, WorkItemPhase::Pending);
        assert_eq!(item.workspace_id, None);
    }

    #[test]
    fn reconcile_unlinks_missing_workspaces() {
        let mut state = state_with("gh", &["a", "b"]);
        state.get_mut("gh:a").expect("item").workspace_id = Some("w1".into());
        state.get_mut("gh:b").expect("item").workspace_id = Some("w2".into());
        state.reconcile_workspaces(&HashSet::from(["w2"]));
        assert_eq!(state.get("gh:a").expect("item").workspace_id, None);
        assert_eq!(
            state.get("gh:b").expect("item").workspace_id.as_deref(),
            Some("w2")
        );
    }

    #[test]
    fn sorted_puts_unseen_first_then_newest() {
        let mut state = WorkItemsState::default();
        state.apply_poll(
            "gh",
            Ok(vec![
                polled("old", "2026-01-01T00:00:00Z"),
                polled("new", "2026-02-01T00:00:00Z"),
                polled("seen", "2026-03-01T00:00:00Z"),
            ]),
        );
        state.mark_seen("gh:seen").expect("item");
        let keys: Vec<&str> = state
            .sorted()
            .iter()
            .map(|item| item.key.as_str())
            .collect();
        assert_eq!(keys, vec!["gh:new", "gh:old", "gh:seen"]);
    }
}
