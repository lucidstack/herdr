//! Turns successive item projections into `work_item.*` events. Pure; no I/O.

use std::collections::HashMap;

use crate::api::schema::WorkItemInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemChange {
    /// New to the inbox, or requested again after it was resolved.
    Created(WorkItemInfo),
    Updated(WorkItemInfo),
    /// The source stopped reporting it; carries the last known state.
    Resolved(WorkItemInfo),
}

/// The last reported state. Until the first `diff` there is no baseline, so items restored
/// at start-up or loaded when a source is enabled are not reported as created.
#[derive(Debug, Default)]
pub(crate) struct ItemChanges {
    baseline: Option<Baseline>,
}

#[derive(Debug)]
struct Baseline {
    revision: u64,
    items: HashMap<String, WorkItemInfo>,
}

impl ItemChanges {
    /// Whether `revision` may carry changes not reported yet.
    pub(crate) fn is_behind(&self, revision: u64) -> bool {
        self.baseline
            .as_ref()
            .is_none_or(|baseline| baseline.revision != revision)
    }

    /// Forgets the baseline; the next `diff` only records the state.
    pub(crate) fn reset(&mut self) {
        self.baseline = None;
    }

    /// Changes since the last call, in projection order, then resolutions.
    pub(crate) fn diff(&mut self, revision: u64, items: Vec<WorkItemInfo>) -> Vec<ItemChange> {
        let current: HashMap<String, WorkItemInfo> = items
            .iter()
            .map(|item| (item.item_id.clone(), item.clone()))
            .collect();
        let Some(mut baseline) = self.baseline.take() else {
            self.baseline = Some(Baseline {
                revision,
                items: current,
            });
            return Vec::new();
        };
        let mut changes = Vec::new();
        for item in items {
            match baseline.items.remove(&item.item_id) {
                None => changes.push(ItemChange::Created(item)),
                Some(previous) if previous == item => {}
                Some(previous) if !previous.resolved && item.resolved => {
                    changes.push(ItemChange::Resolved(item))
                }
                Some(previous) if previous.resolved && !item.resolved => {
                    changes.push(ItemChange::Created(item))
                }
                Some(_) => changes.push(ItemChange::Updated(item)),
            }
        }
        // Whatever is left was dropped; an already resolved item was reported before.
        let mut dropped: Vec<WorkItemInfo> = baseline
            .items
            .into_values()
            .filter(|item| !item.resolved)
            .collect();
        dropped.sort_by(|a, b| a.item_id.cmp(&b.item_id));
        changes.extend(dropped.into_iter().map(ItemChange::Resolved));
        self.baseline = Some(Baseline {
            revision,
            items: current,
        });
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::WorkItemPhase;

    fn item(id: &str) -> WorkItemInfo {
        WorkItemInfo {
            item_id: id.into(),
            source_id: "gh".into(),
            context: id.into(),
            title: "t".into(),
            author: None,
            url: "u".into(),
            summary: None,
            notice: None,
            phase: WorkItemPhase::Pending,
            seen: false,
            resolved: false,
            workspace_id: None,
            dismissed: false,
            snoozed_until: None,
            choices: Vec::new(),
            default_choice_id: None,
            provisioning: None,
        }
    }

    fn baseline(items: Vec<WorkItemInfo>) -> ItemChanges {
        let mut changes = ItemChanges::default();
        assert!(changes.diff(1, items).is_empty());
        changes
    }

    #[test]
    fn the_first_state_is_recorded_without_events() {
        let mut changes = ItemChanges::default();
        assert!(changes.is_behind(0));
        assert!(changes.diff(3, vec![item("a")]).is_empty());
        assert!(!changes.is_behind(3));
    }

    #[test]
    fn new_changed_and_dropped_items_are_reported() {
        let mut changes = baseline(vec![item("a"), item("b")]);
        let seen = WorkItemInfo {
            seen: true,
            ..item("a")
        };
        assert_eq!(
            changes.diff(2, vec![seen.clone(), item("c")]),
            vec![
                ItemChange::Updated(seen),
                ItemChange::Created(item("c")),
                ItemChange::Resolved(item("b")),
            ]
        );
    }

    #[test]
    fn resolution_of_a_kept_item_is_reported_once_and_rerequest_creates_it() {
        let mut changes = baseline(vec![item("a")]);
        let resolved = WorkItemInfo {
            resolved: true,
            ..item("a")
        };
        assert_eq!(
            changes.diff(2, vec![resolved.clone()]),
            vec![ItemChange::Resolved(resolved.clone())]
        );
        // Closing its workspace drops it without a second resolution.
        assert!(changes.diff(3, Vec::new()).is_empty());
        assert_eq!(
            changes.diff(4, vec![item("a")]),
            vec![ItemChange::Created(item("a"))]
        );
    }

    #[test]
    fn reset_rebaselines_without_events() {
        let mut changes = baseline(vec![item("a")]);
        changes.reset();
        assert!(changes.diff(2, vec![item("b")]).is_empty());
        assert_eq!(
            changes.diff(3, Vec::new()),
            vec![ItemChange::Resolved(item("b"))]
        );
    }
}
