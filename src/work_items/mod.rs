//! Work items: tasks from external sources that exist before any workspace and
//! own the workspace once one is provisioned.
//!
//! The runtime holder here is source-agnostic; sources implement
//! [`source::WorkItemSource`]. Nothing runs unless a source is configured.

pub(crate) mod github;
pub(crate) mod process;
pub(crate) mod provision;
pub(crate) mod source;
pub(crate) mod state;
pub(crate) mod store;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::api::schema::{
    WorkItemInfo, WorkItemPhase, WorkItemProvisioningInfo, WorkItemSourceInfo,
};
use crate::config::WorkItemsConfig;

use provision::ProvisionJob;
pub(crate) use source::{PreparedItem, ProvisionPlan, SourceItem, WorkItemSource};
use state::{NotFound, WorkItem, WorkItemsState};
use store::StoreWriter;

#[derive(Debug)]
pub(crate) enum WorkItemsEvent {
    Polled {
        source_id: String,
        result: Result<Vec<SourceItem>, String>,
    },
    Prepared {
        key: String,
        updated_at: String,
        prepared: PreparedItem,
    },
    CheckoutFinished {
        job_id: u64,
        result: Result<provision::SourceReady, String>,
    },
}

/// A review worktree created for an item, remembered so its branch can be cleaned up
/// when Herdr removes the worktree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct OwnedWorktree {
    pub checkout_path: String,
    pub repo_path: PathBuf,
    pub branch: String,
    pub delete_branch: bool,
}

/// A workspace being removed because its item resolved.
#[derive(Debug)]
pub(crate) struct PendingRemoval {
    pub key: String,
    pub pending: provision::PendingResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkItemNotice {
    pub title: String,
    pub body: Option<String>,
}

pub(crate) struct WorkItems {
    sources: Vec<Arc<dyn WorkItemSource>>,
    config: WorkItemsConfig,
    state: WorkItemsState,
    revision: u64,
    next_poll: HashMap<String, Instant>,
    polls_in_flight: HashSet<String>,
    store: Option<StoreWriter>,
    loaded: bool,
    /// Running local provisioning, keyed by item key.
    jobs: HashMap<String, ProvisionJob>,
    next_job_id: u64,
    /// Provisioning notices waiting for delivery to client shells.
    notices: Vec<WorkItemNotice>,
    owned_worktrees: Vec<OwnedWorktree>,
    /// Resolved items whose workspace should be removed, waiting for the app.
    pending_resolutions: Vec<String>,
    removals: Vec<PendingRemoval>,
}

impl std::fmt::Debug for WorkItems {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkItems")
            .field("sources", &self.sources.len())
            .field("revision", &self.revision)
            .field("items", &self.state.items().len())
            .finish()
    }
}

/// How the work-item store is backed for this server.
#[derive(Debug, Clone)]
pub(crate) struct StorePolicy {
    pub path: PathBuf,
    pub load: bool,
    pub persist: bool,
}

fn build_sources(config: &WorkItemsConfig) -> Vec<Arc<dyn WorkItemSource>> {
    let mut sources: Vec<Arc<dyn WorkItemSource>> = Vec::new();
    if let Some(github) = config.github.as_ref().filter(|github| github.enabled) {
        sources.push(Arc::new(github::GithubSource::new(github.clone())));
    }
    sources
}

impl WorkItems {
    pub(crate) fn disabled() -> Self {
        Self {
            sources: Vec::new(),
            config: WorkItemsConfig::default(),
            state: WorkItemsState::default(),
            revision: 0,
            next_poll: HashMap::new(),
            polls_in_flight: HashSet::new(),
            store: None,
            loaded: false,
            jobs: HashMap::new(),
            next_job_id: 1,
            notices: Vec::new(),
            owned_worktrees: Vec::new(),
            pending_resolutions: Vec::new(),
            removals: Vec::new(),
        }
    }

    pub(crate) fn from_config(
        config: &WorkItemsConfig,
        store: StorePolicy,
        existing_workspace_ids: &HashSet<&str>,
        now: Instant,
    ) -> Self {
        let mut items = Self::disabled();
        items.config = config.clone();
        items.sources = build_sources(config);
        if items.sources.is_empty() {
            return items;
        }
        items.enable(&store, existing_workspace_ids, now);
        items
    }

    fn enable(
        &mut self,
        store: &StorePolicy,
        existing_workspace_ids: &HashSet<&str>,
        now: Instant,
    ) {
        if !self.loaded {
            self.loaded = true;
            if store.load {
                let stored = store::load(&store.path);
                self.state = WorkItemsState::from_items(stored.items);
                self.owned_worktrees = stored.worktrees;
            }
            if store.persist {
                self.store = StoreWriter::spawn(store.path.clone());
            }
            self.state.reconcile_workspaces(existing_workspace_ids);
        }
        self.retain_configured_sources();
        self.schedule_all(now);
        self.revision = self.revision.max(1);
    }

    fn retain_configured_sources(&mut self) -> bool {
        let ids: HashSet<&str> = self.sources.iter().map(|source| source.id()).collect();
        self.state.retain_sources(&ids)
    }

    fn schedule_all(&mut self, now: Instant) {
        self.next_poll = self
            .sources
            .iter()
            .map(|source| (source.id().to_string(), now))
            .collect();
    }

    /// Applies a live config reload. `store` is only used when enabling for the first time.
    pub(crate) fn apply_config(
        &mut self,
        config: &WorkItemsConfig,
        store: StorePolicy,
        existing_workspace_ids: &HashSet<&str>,
        now: Instant,
    ) {
        if *config == self.config {
            return;
        }
        self.config = config.clone();
        self.sources = build_sources(config);
        self.polls_in_flight.clear();
        if self.sources.is_empty() {
            self.next_poll.clear();
            if self.revision > 0 {
                self.retain_configured_sources();
                self.changed();
            }
            return;
        }
        self.enable(&store, existing_workspace_ids, now);
        self.changed();
    }

    pub(crate) fn is_enabled(&self) -> bool {
        !self.sources.is_empty()
    }

    /// Bumped on every visible change; 0 while no source was ever enabled.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_poll
            .iter()
            .filter(|(id, _)| !self.polls_in_flight.contains(*id))
            .map(|(_, deadline)| *deadline)
            .chain(self.jobs.values().filter_map(ProvisionJob::next_attempt))
            .chain(
                self.removals
                    .iter()
                    .map(|removal| removal.pending.next_check),
            )
            .min()
    }

    pub(crate) fn source(&self, source_id: &str) -> Option<&Arc<dyn WorkItemSource>> {
        self.sources.iter().find(|source| source.id() == source_id)
    }

    /// Sources whose poll is due; marks them in flight.
    pub(crate) fn take_due_polls(&mut self, now: Instant) -> Vec<Arc<dyn WorkItemSource>> {
        let due: Vec<Arc<dyn WorkItemSource>> = self
            .sources
            .iter()
            .filter(|source| {
                !self.polls_in_flight.contains(source.id())
                    && self
                        .next_poll
                        .get(source.id())
                        .is_some_and(|deadline| *deadline <= now)
            })
            .cloned()
            .collect();
        for source in &due {
            self.polls_in_flight.insert(source.id().to_string());
        }
        due
    }

    /// Items of `source_id` that need background preparation; marks them in flight.
    pub(crate) fn take_needs_prepare(&mut self, source_id: &str) -> Vec<SourceItem> {
        self.state.needs_prepare(source_id)
    }

    pub(crate) fn apply_event(
        &mut self,
        event: WorkItemsEvent,
        now: Instant,
    ) -> (bool, Vec<WorkItemNotice>) {
        match event {
            WorkItemsEvent::Polled { source_id, result } => {
                self.polls_in_flight.remove(&source_id);
                let Some(source) = self.source(&source_id).cloned() else {
                    return (false, Vec::new());
                };
                self.next_poll
                    .insert(source_id.clone(), now + source.poll_interval());
                let (changed, arrivals) = self.state.apply_poll(&source_id, result);
                // One notice per poll: a first poll can report many items at once.
                let notices = arrivals
                    .first()
                    .and_then(|key| self.state.get(key))
                    .map(|item| {
                        let (title, body) = source.arrival_notice(&item.source_item());
                        let body = if arrivals.len() > 1 {
                            Some(format!("{} new from {}", arrivals.len(), source.label()))
                        } else {
                            body
                        };
                        WorkItemNotice { title, body }
                    })
                    .into_iter()
                    .collect();
                for key in self.state.take_newly_resolved() {
                    if self
                        .state
                        .get(&key)
                        .is_some_and(|item| source.remove_on_resolved(item))
                    {
                        self.pending_resolutions.push(key);
                    }
                }
                if changed {
                    self.changed();
                }
                (changed, notices)
            }
            WorkItemsEvent::Prepared {
                key,
                updated_at,
                prepared,
            } => {
                let changed = self.state.apply_prepared(&key, &updated_at, prepared);
                if changed {
                    self.changed();
                }
                (changed, Vec::new())
            }
            // Provisioning results need the app and are handled by its driver.
            WorkItemsEvent::CheckoutFinished { .. } => (false, Vec::new()),
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&WorkItem> {
        self.state.get(key)
    }

    pub(crate) fn mark_seen(&mut self, key: &str) -> Result<(), NotFound> {
        if self.state.mark_seen(key)? {
            self.changed();
        }
        Ok(())
    }

    pub(crate) fn set_awaiting_external(&mut self, key: &str) -> Result<(), NotFound> {
        if self.state.set_awaiting_external(key)? {
            self.changed();
        }
        Ok(())
    }

    pub(crate) fn workspace_closed(&mut self, workspace_id: &str) {
        // Late worker results for a dropped job are ignored by job id.
        self.jobs
            .retain(|_, job| job.workspace_id.as_deref() != Some(workspace_id));
        if self.state.workspace_closed(workspace_id) {
            self.changed();
        }
    }

    pub(crate) fn has_job(&self, key: &str) -> bool {
        self.jobs.contains_key(key)
    }

    /// Starts provisioning `key`; returns the new job id.
    pub(crate) fn start_job(&mut self, key: &str, plan: ProvisionPlan) -> Result<u64, NotFound> {
        let item = self.state.get_mut(key).ok_or(NotFound)?;
        item.phase = WorkItemPhase::Local;
        item.seen = true;
        item.provisioning = Some(provision::initial_progress(&plan));
        let job_id = self.next_job_id;
        self.next_job_id += 1;
        self.jobs.insert(
            key.to_string(),
            ProvisionJob {
                job_id,
                key: key.to_string(),
                plan,
                workspace_id: None,
                agent_pane_id: None,
                agent_name: None,
                branch: None,
                worktree: None,
                agent_start: None,
                brief: None,
                brief_confirmation: None,
            },
        );
        self.changed();
        Ok(job_id)
    }

    /// Resolved items whose workspace the app should now remove.
    pub(crate) fn take_pending_resolutions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_resolutions)
    }

    pub(crate) fn start_removal(&mut self, removal: PendingRemoval) {
        self.removals.push(removal);
    }

    pub(crate) fn removals_mut(&mut self) -> &mut Vec<PendingRemoval> {
        &mut self.removals
    }

    /// Records why an item's workspace could not be removed on resolution.
    pub(crate) fn set_resolve_error(&mut self, key: &str, error: Option<String>) {
        if let Some(item) = self.state.get_mut(key) {
            if item.resolve_error != error {
                item.resolve_error = error;
                self.changed();
            }
        }
    }

    pub(crate) fn record_owned_worktree(&mut self, worktree: OwnedWorktree) {
        self.owned_worktrees
            .retain(|owned| owned.checkout_path != worktree.checkout_path);
        self.owned_worktrees.push(worktree);
        self.changed();
    }

    /// Forgets and returns the review worktree at `checkout_path`, if one was created here.
    pub(crate) fn take_owned_worktree(&mut self, checkout_path: &str) -> Option<OwnedWorktree> {
        let canonical = |path: &str| crate::worktree::canonical_or_original(Path::new(path));
        let wanted = canonical(checkout_path);
        let index = self
            .owned_worktrees
            .iter()
            .position(|owned| canonical(&owned.checkout_path) == wanted)?;
        let owned = self.owned_worktrees.remove(index);
        self.changed();
        Some(owned)
    }

    pub(crate) fn job(&self, job_id: u64) -> Option<&ProvisionJob> {
        self.jobs.values().find(|job| job.job_id == job_id)
    }

    pub(crate) fn job_mut(&mut self, job_id: u64) -> Option<&mut ProvisionJob> {
        self.jobs.values_mut().find(|job| job.job_id == job_id)
    }

    pub(crate) fn job_ids(&self) -> Vec<u64> {
        self.jobs.values().map(|job| job.job_id).collect()
    }

    /// Applies `update` to the progress of the job's item.
    pub(crate) fn update_progress(
        &mut self,
        job_id: u64,
        update: impl FnOnce(&mut WorkItemProvisioningInfo),
    ) {
        let Some(key) = self.job(job_id).map(|job| job.key.clone()) else {
            return;
        };
        let Some(progress) = self
            .state
            .get_mut(&key)
            .and_then(|item| item.provisioning.as_mut())
        else {
            return;
        };
        let before = progress.clone();
        update(progress);
        if *progress != before {
            self.changed();
        }
    }

    pub(crate) fn progress(&self, job_id: u64) -> Option<&WorkItemProvisioningInfo> {
        let job = self.job(job_id)?;
        self.state.get(&job.key)?.provisioning.as_ref()
    }

    /// Records the provisioned workspace on the job and its item.
    pub(crate) fn link_workspace(&mut self, job_id: u64, workspace_id: &str) {
        let Some(job) = self.job_mut(job_id) else {
            return;
        };
        job.workspace_id = Some(workspace_id.to_string());
        let key = job.key.clone();
        if let Some(item) = self.state.get_mut(&key) {
            item.workspace_id = Some(workspace_id.to_string());
            self.changed();
        }
    }

    pub(crate) fn take_notices(&mut self) -> Vec<WorkItemNotice> {
        std::mem::take(&mut self.notices)
    }

    /// Removes a job whose steps are all terminal and queues its notice.
    pub(crate) fn finish_job_if_done(&mut self, job_id: u64) {
        if let Some(notice) = self.finished_job_notice(job_id) {
            self.notices.push(notice);
        }
    }

    fn finished_job_notice(&mut self, job_id: u64) -> Option<WorkItemNotice> {
        let progress = self.progress(job_id)?;
        if !progress.finished {
            return None;
        }
        let failed = provision::has_failure(progress);
        let key = self.job(job_id)?.key.clone();
        self.jobs.remove(&key);
        let item = self.state.get_mut(&key)?;
        if item.workspace_id.is_none() {
            item.phase = WorkItemPhase::Pending;
            self.changed();
        }
        let context = self.state.get(&key).map(|item| item.context.clone());
        let title = match (failed, self.state.get(&key)?.workspace_id.is_some()) {
            (false, _) => "Review workspace ready",
            (true, true) => "Review workspace finished with errors",
            (true, false) => "Review workspace failed",
        };
        Some(WorkItemNotice {
            title: title.into(),
            body: context,
        })
    }

    pub(crate) fn projection_items(&self) -> Vec<WorkItemInfo> {
        self.state
            .sorted()
            .into_iter()
            .map(|item| {
                let choices = self
                    .source(&item.source_id)
                    .map(|source| source.choices(item))
                    .unwrap_or(source::ItemChoices {
                        choices: Vec::new(),
                        default_choice_id: None,
                    });
                item.info(choices)
            })
            .collect()
    }

    pub(crate) fn source_infos(&self) -> Vec<WorkItemSourceInfo> {
        self.sources
            .iter()
            .map(|source| WorkItemSourceInfo {
                source_id: source.id().to_string(),
                label: source.label().to_string(),
                error: self.state.source_error(source.id()).map(str::to_string),
            })
            .collect()
    }

    fn changed(&mut self) {
        self.revision += 1;
        if let Some(store) = &self.store {
            store.save(self.state.items(), &self.owned_worktrees);
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(sources: Vec<Arc<dyn WorkItemSource>>, now: Instant) -> Self {
        let mut items = Self::disabled();
        items.sources = sources;
        items.loaded = true;
        items.schedule_all(now);
        items.revision = 1;
        items
    }

    #[cfg(test)]
    pub(crate) fn schedule_all_for_test(&mut self, now: Instant) {
        self.schedule_all(now);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{source_item, FakeSource};
    use super::*;

    fn poll(items: &mut WorkItems, ids: &[&str]) -> Vec<WorkItemNotice> {
        items
            .apply_event(
                WorkItemsEvent::Polled {
                    source_id: "fake".into(),
                    result: Ok(ids.iter().map(|id| source_item(id)).collect()),
                },
                Instant::now(),
            )
            .1
    }

    #[test]
    fn single_arrival_uses_the_source_notice() {
        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        assert_eq!(
            poll(&mut items, &["a"]),
            vec![WorkItemNotice {
                title: "Arrived".into(),
                body: Some("Title a".into()),
            }]
        );
    }

    #[test]
    fn many_arrivals_in_one_poll_collapse_into_one_notice() {
        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        assert_eq!(
            poll(&mut items, &["a", "b", "c"]),
            vec![WorkItemNotice {
                title: "Arrived".into(),
                body: Some("3 new from Fake".into()),
            }]
        );
    }
}
