//! Work items: tasks from external sources that exist before any workspace and
//! own the workspace once one is provisioned.
//!
//! The runtime holder here is source-agnostic; sources implement
//! [`source::WorkItemSource`]. Nothing runs unless a source is configured.

pub(crate) mod github;
pub(crate) mod process;
pub(crate) mod source;
pub(crate) mod state;
pub(crate) mod store;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::api::schema::{WorkItemInfo, WorkItemSourceInfo};
use crate::config::WorkItemsConfig;

pub(crate) use source::{PreparedItem, SourceItem, WorkItemSource};
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
                self.state = WorkItemsState::from_items(store::load(&store.path));
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

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_poll
            .iter()
            .filter(|(id, _)| !self.polls_in_flight.contains(*id))
            .map(|(_, deadline)| *deadline)
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
                let notices = arrivals
                    .iter()
                    .filter_map(|key| self.state.get(key))
                    .map(|item| {
                        let (title, body) = source.arrival_notice(&item.source_item());
                        WorkItemNotice { title, body }
                    })
                    .collect();
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
        if self.state.workspace_closed(workspace_id) {
            self.changed();
        }
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
            store.save(self.state.items());
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
