//! Work items: tasks from external sources that exist before any workspace and
//! own the workspace once one is provisioned.
//!
//! The runtime holder here is source-agnostic; sources implement
//! [`source::WorkItemSource`]. Nothing runs unless a source is configured.

pub(crate) mod changes;
pub(crate) mod github;
pub(crate) mod jira;
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::schema::{
    WorkItemInfo, WorkItemPhase, WorkItemProvisioningInfo, WorkItemSourceInfo,
};
use crate::config::WorkItemsConfig;

pub(crate) use changes::{ItemChange, ItemChanges};
use provision::ProvisionJob;
pub(crate) use source::{PreparedItem, ProvisionPlan, SourceItem, TicketDetail, WorkItemSource};
use state::{NotFound, WorkItem, WorkItemsState};
use store::StoreWriter;

/// Current wall-clock time in Unix seconds; 0 if the clock is before 1970.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

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
    /// A `Perform` choice finished on its background thread.
    Performed {
        key: String,
        result: Result<String, String>,
    },
    /// A `work_item.add` fetch finished; the item is inserted only now that it succeeded,
    /// so its API response carries the resulting item.
    TicketFetchedForAdd {
        id: String,
        source_id: String,
        result: Result<Box<TicketDetail>, String>,
        respond_to: std::sync::mpsc::Sender<String>,
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

/// Persisted "Pick next" memory: which provider was used last, and each provider's last
/// context text, so the dialog can pre-fill without depending on item recency.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PickNextState {
    #[serde(default)]
    pub last_source_id: Option<String>,
    #[serde(default)]
    pub last_context: HashMap<String, String>,
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
    /// Follow-up briefs sent to an item's agent, waiting for the `agent.prompt` response.
    follow_ups: Vec<(String, std::sync::mpsc::Receiver<String>)>,
    pick_next: PickNextState,
    /// Work items moved focus to a workspace outside an API request, e.g. a new
    /// "Pick next" workspace; the server moves its shell clients there too.
    focus_requested: bool,
    /// Mapped local clones, reachable from the inbox even without an item.
    repositories: Vec<Repository>,
}

/// A mapped local clone and the Git key that recognises workspaces on its main checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Repository {
    pub info: crate::api::schema::WorkItemRepositoryInfo,
    /// `None` when the path is not a Git repository: it then never has a home.
    pub repo_key: Option<String>,
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
    if let Some(jira) = config.jira.as_ref().filter(|jira| jira.enabled) {
        sources.push(Arc::new(jira::JiraSource::new(jira.clone())));
    }
    sources
}

/// The local clones mapped by enabled sources, one per path, in configuration order.
fn build_repositories(config: &WorkItemsConfig) -> Vec<Repository> {
    let github = config
        .github
        .as_ref()
        .filter(|github| github.enabled)
        .into_iter()
        .flat_map(|github| github.repos.iter().map(|repo| repo.path.as_str()));
    let jira = config
        .jira
        .as_ref()
        .filter(|jira| jira.enabled)
        .into_iter()
        .flat_map(|jira| jira.projects.iter().map(|project| project.path.as_str()));
    let mut seen = HashSet::new();
    github
        .chain(jira)
        .filter(|path| !path.trim().is_empty())
        .map(crate::worktree::expand_tilde_absolute_path)
        .filter(|path| seen.insert(crate::worktree::canonical_or_original(path)))
        .map(|path| Repository {
            // Once per config load, not per projection: this asks Git.
            repo_key: crate::workspace::git_space_metadata(&path).map(|space| space.key),
            info: crate::api::schema::WorkItemRepositoryInfo {
                label: path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
                path: path.display().to_string(),
                workspace_id: None,
            },
        })
        .collect()
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
            follow_ups: Vec::new(),
            pick_next: PickNextState::default(),
            focus_requested: false,
            repositories: Vec::new(),
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
        items.repositories = build_repositories(config);
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
                self.pick_next = stored.pick_next;
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
        self.repositories = build_repositories(config);
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
        // Snoozes are stored as wall-clock times so they survive restarts.
        let snooze_end = self
            .state
            .next_snooze_end()
            .map(|until| Instant::now() + Duration::from_secs(until.saturating_sub(unix_now())));
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
            .chain(snooze_end)
            .min()
    }

    /// Brings back items whose snooze ended. Returns whether anything changed.
    pub(crate) fn expire_snoozes(&mut self) -> bool {
        let changed = self.state.expire_snoozes(unix_now());
        if changed {
            self.changed();
        }
        changed
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
                self.queue_resolutions();
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
                let (changed, review_arrived) =
                    self.state.apply_prepared(&key, &updated_at, prepared);
                self.queue_resolutions();
                if changed {
                    self.changed();
                }
                let notices = review_arrived
                    .then(|| self.state.get(&key))
                    .flatten()
                    .map(|item| WorkItemNotice {
                        title: "New review on your pull request".into(),
                        body: Some(item.context.clone()),
                    })
                    .into_iter()
                    .collect();
                (changed, notices)
            }
            // Provisioning results need the app and are handled by its driver.
            WorkItemsEvent::CheckoutFinished { .. } => (false, Vec::new()),
            WorkItemsEvent::Performed { key, result } => {
                let notices: Vec<WorkItemNotice> =
                    self.finish_action(&key, result, now).into_iter().collect();
                (!notices.is_empty(), notices)
            }
            // Handled by the app driver, which owns the response channel.
            WorkItemsEvent::TicketFetchedForAdd { .. } => (false, Vec::new()),
        }
    }

    /// Queues workspace removal for items that just resolved, when their source wants that.
    fn queue_resolutions(&mut self) {
        for key in self.state.take_newly_resolved() {
            let should_remove = self
                .state
                .get(&key)
                .and_then(|item| {
                    self.source(&item.source_id)
                        .map(|source| source.remove_on_resolved(item))
                })
                .unwrap_or(false);
            if should_remove {
                self.pending_resolutions.push(key);
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

    /// Dismisses (`until` is `None`) or snoozes an item until a Unix time.
    pub(crate) fn hide(&mut self, key: &str, until: Option<u64>) -> Result<(), NotFound> {
        if self.state.hide(key, until)? {
            self.changed();
        }
        Ok(())
    }

    pub(crate) fn unhide(&mut self, key: &str) -> Result<(), NotFound> {
        if self.state.unhide(key)? {
            self.changed();
        }
        Ok(())
    }

    /// Marks a `Perform` choice as running: the item shows a spinner until the source stops
    /// reporting it. Fails while one is already running.
    pub(crate) fn begin_action(&mut self, key: &str) -> Result<(), &'static str> {
        let item = self.state.get_mut(key).ok_or("work_item_not_found")?;
        if item.action_in_flight {
            return Err("work_item_busy");
        }
        item.action_in_flight = true;
        item.action_error = None;
        item.phase_before_action = Some(item.phase);
        item.phase = WorkItemPhase::AwaitingExternal;
        item.seen = true;
        self.changed();
        Ok(())
    }

    /// Records a finished `Perform` choice. Success polls the source at once, so the item
    /// leaves the inbox as soon as the source agrees; failure puts it back with the reason.
    pub(crate) fn finish_action(
        &mut self,
        key: &str,
        result: Result<String, String>,
        now: Instant,
    ) -> Option<WorkItemNotice> {
        let item = self.state.get_mut(key)?;
        item.action_in_flight = false;
        let before = item.phase_before_action.take();
        let context = item.context.clone();
        let source_id = item.source_id.clone();
        let notice = match result {
            Ok(message) => {
                // Work in its own workspace carries on (e.g. after updating the tracker);
                // anything else waits for the source to drop the item (e.g. a merge).
                if before == Some(WorkItemPhase::Local) {
                    item.phase = WorkItemPhase::Local;
                }
                self.next_poll.insert(source_id, now);
                WorkItemNotice {
                    title: message,
                    body: Some(context),
                }
            }
            Err(error) => {
                item.phase = before.unwrap_or(WorkItemPhase::Pending);
                item.action_error = Some(error.clone());
                WorkItemNotice {
                    title: "Could not complete the action".into(),
                    body: Some(format!("{context} · {error}")),
                }
            }
        };
        self.changed();
        Some(notice)
    }

    /// Attaches an existing workspace to an item; a running provisioning job is dropped
    /// (its late results are ignored by job id).
    pub(crate) fn link(&mut self, key: &str, workspace_id: &str) -> Result<(), NotFound> {
        if self.state.link(key, workspace_id)? {
            self.jobs.remove(key);
            self.changed();
        }
        Ok(())
    }

    /// Inserts a fetched ticket into the inbox as a hand-added item, exempt from poll
    /// resolution until the tracker reports it done or the user dismisses it. A ticket
    /// already in the inbox is returned unchanged.
    pub(crate) fn add_ticket(&mut self, source_id: &str, item: SourceItem) -> WorkItem {
        let (item, inserted) = self.state.insert_manual(source_id, item);
        if inserted {
            self.changed();
        }
        item
    }

    /// The key of `source_id`'s "Pick next" discovery row, creating it if it does not exist
    /// yet.
    pub(crate) fn ensure_pick_next_item(&mut self, source_id: &str, label: &str) -> WorkItem {
        let (item, inserted) = self.state.ensure_pick_next(source_id, label);
        if inserted {
            self.changed();
        }
        item
    }

    /// Records the text last sent to `source_id`'s "Pick next" discovery row, and that
    /// `source_id` was used last, as explicit persisted state (not derived from item
    /// recency), so the dialog can pre-fill correctly across restarts.
    pub(crate) fn set_pick_next_context(&mut self, key: &str, source_id: &str, context: &str) {
        self.state.set_pick_next_context(key, context, unix_now());
        self.pick_next.last_source_id = Some(source_id.to_string());
        self.pick_next
            .last_context
            .insert(source_id.to_string(), context.to_string());
        self.changed();
    }

    /// Records that work items focused a workspace on their own.
    pub(crate) fn request_focus(&mut self) {
        self.focus_requested = true;
    }

    /// Whether work items focused a workspace since the last call.
    pub(crate) fn take_focus_request(&mut self) -> bool {
        std::mem::take(&mut self.focus_requested)
    }

    /// Persisted "Pick next" memory for the dialog to pre-fill from.
    pub(crate) fn pick_next_info(&self) -> crate::api::schema::WorkItemPickNextInfo {
        crate::api::schema::WorkItemPickNextInfo {
            last_source_id: self.pick_next.last_source_id.clone(),
            last_context: self.pick_next.last_context.clone(),
        }
    }

    /// The projected info for one item, if it exists.
    pub(crate) fn item_info(&self, key: &str) -> Option<WorkItemInfo> {
        self.state.get(key).map(|item| self.project(item))
    }

    fn project(&self, item: &WorkItem) -> WorkItemInfo {
        let (choices, reminder) = self.choices_and_reminder(item);
        item.info(choices, reminder.map(|reminder| reminder.message))
    }

    /// The choices offered for `item`, including the start reminder's.
    pub(crate) fn item_choices(&self, item: &WorkItem) -> source::ItemChoices {
        self.choices_and_reminder(item).0
    }

    /// The source's choices, plus, while the item has a workspace and its tracker lags
    /// behind, the reminder's fix (the default when available) and a way to mute it.
    fn choices_and_reminder(
        &self,
        item: &WorkItem,
    ) -> (source::ItemChoices, Option<source::StartReminder>) {
        let Some(source) = self.source(&item.source_id) else {
            return (
                source::ItemChoices {
                    choices: Vec::new(),
                    default_choice_id: None,
                },
                None,
            );
        };
        let mut choices = source.choices(item);
        let reminder = (item.workspace_id.is_some() && !item.start_reminder_muted)
            .then(|| source.start_reminder(item))
            .flatten();
        if let Some(reminder) = &reminder {
            if reminder.choice.disabled_reason.is_none() {
                choices.default_choice_id = Some(reminder.choice.choice_id.clone());
            }
            choices.choices.insert(0, reminder.choice.clone());
            choices.choices.insert(
                1,
                crate::api::schema::WorkItemChoiceInfo {
                    choice_id: source::MUTE_START_REMINDER_CHOICE_ID.into(),
                    label: "Don't remind me for this ticket".into(),
                    description: Some("Keep working without updating the tracker".into()),
                    action: crate::api::schema::WorkItemChoiceAction::Perform,
                    disabled_reason: None,
                    confirm: None,
                },
            );
        }
        (choices, reminder)
    }

    /// "Don't remind me for this ticket".
    pub(crate) fn mute_start_reminder(&mut self, key: &str) -> Result<(), NotFound> {
        let item = self.state.get_mut(key).ok_or(NotFound)?;
        if !item.start_reminder_muted {
            item.start_reminder_muted = true;
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

    /// Tracks a follow-up brief whose `agent.prompt` response arrives on `response`.
    pub(crate) fn start_follow_up(
        &mut self,
        key: &str,
        response: std::sync::mpsc::Receiver<String>,
    ) {
        if let Some(item) = self.state.get_mut(key) {
            item.action_error = None;
        }
        self.follow_ups.push((key.to_string(), response));
        self.changed();
    }

    /// Drains finished follow-up responses; `parse` returns why one failed, if it did.
    pub(crate) fn finish_follow_ups(&mut self, parse: impl Fn(&str) -> Option<String>) {
        let mut failures = Vec::new();
        self.follow_ups.retain(|(key, response)| {
            let failure = match response.try_recv() {
                Ok(response) => parse(&response),
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some("agent prompt response was lost".to_string())
                }
            };
            failures.extend(failure.map(|error| (key.clone(), error)));
            false
        });
        if failures.is_empty() {
            return;
        }
        for (key, error) in failures {
            if let Some(item) = self.state.get_mut(&key) {
                item.action_error = Some(error);
            }
        }
        self.changed();
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

    /// Whether the worktree at `checkout_path` was created for an item (not adopted).
    pub(crate) fn owns_worktree(&self, checkout_path: &Path) -> bool {
        let wanted = crate::worktree::canonical_or_original(checkout_path);
        self.owned_worktrees.iter().any(|owned| {
            crate::worktree::canonical_or_original(Path::new(&owned.checkout_path)) == wanted
        })
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
            .map(|item| self.project(item))
            .collect()
    }

    pub(crate) fn repository_infos(&self) -> Vec<crate::api::schema::WorkItemRepositoryInfo> {
        self.repositories
            .iter()
            .map(|repository| repository.info.clone())
            .collect()
    }

    /// Records each repository's home, the workspace `home_of` finds for its Git key.
    /// Cheap enough for every loop turn: it allocates only when a home changes.
    pub(crate) fn update_repository_homes<'a>(
        &mut self,
        home_of: impl Fn(&str) -> Option<&'a str>,
    ) -> bool {
        let mut changed = false;
        for repository in &mut self.repositories {
            let home = repository.repo_key.as_deref().and_then(&home_of);
            if repository.info.workspace_id.as_deref() != home {
                repository.info.workspace_id = home.map(str::to_string);
                changed = true;
            }
        }
        if changed {
            self.changed();
        }
        changed
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
            store.save(self.state.items(), &self.owned_worktrees, &self.pick_next);
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

    #[test]
    fn repositories_come_from_enabled_sources_once_per_path() {
        let config: WorkItemsConfig = toml::from_str(
            r#"
[github]
repos = [
    { name = "o/app", path = "/src/app" },
    { name = "o/api", path = "/src/api" },
]

[jira]
site = "example.atlassian.net"
email = "me@example.com"
projects = [
    { key = "APP", path = "/src/app" },
    { key = "OPS", path = "/src/ops" },
]
"#,
        )
        .expect("config");
        let paths = |config: &WorkItemsConfig| -> Vec<(String, String)> {
            build_repositories(config)
                .into_iter()
                .map(|repository| (repository.info.path, repository.info.label))
                .collect()
        };
        assert_eq!(
            paths(&config),
            [
                ("/src/app", "app"),
                ("/src/api", "api"),
                ("/src/ops", "ops")
            ]
            .map(|(path, label)| (path.to_string(), label.to_string()))
        );
        let mut jira_off = config.clone();
        jira_off.jira.as_mut().expect("jira").enabled = false;
        assert_eq!(paths(&jira_off).len(), 2);
    }

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
