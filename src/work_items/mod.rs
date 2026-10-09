//! Work items: tasks from external sources that exist before any workspace and
//! own the workspace once one is provisioned.
//!
//! The runtime holder here is source-agnostic; sources implement
//! [`source::WorkItemSource`]. Nothing runs unless a source is configured.

mod adf;
pub(crate) mod agent_settings;
pub(crate) mod attention;
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
    AttentionInfo, AttentionKind, WorkItemActionOutcome, WorkItemInfo, WorkItemPhase,
    WorkItemProject, WorkItemProvisioningInfo, WorkItemSourceInfo,
};
use crate::config::{GithubRepoConfig, WorkItemsConfig};

pub(crate) use changes::{ItemChange, ItemChanges};
use provision::ProvisionJob;
use source::LinkedClone;
pub(crate) use source::{
    PreparedItem, ProvisionPlan, SourceItem, TicketDetail, TicketImage, WorkItemSource,
};
use state::{NotFound, WorkItem, WorkItemsState};
use store::StoreWriter;

/// Current wall-clock time in Unix seconds; 0 if the clock is before 1970.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `unix` seconds as an RFC 3339 UTC time, the form GitHub gives its `updated_at` in, so that
/// items of every source sort by recency together. The Unix time itself when out of range.
fn rfc3339_utc(unix: u64) -> String {
    i64::try_from(unix)
        .ok()
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| unix.to_string())
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
    /// A link-clone choice finished on its background thread.
    CloneLinked {
        key: String,
        result: Result<(LinkedClone, String), String>,
    },
    /// A `work_item.add` fetch finished; the item is inserted only now that it succeeded,
    /// so its API response carries the resulting item.
    TicketFetchedForAdd {
        id: String,
        source_id: String,
        result: Result<Box<TicketDetail>, String>,
        respond_to: std::sync::mpsc::Sender<String>,
    },
    /// Pull requests looked up for items' workspace branches, per item key, and the state
    /// of pull request items' own pull requests, per item key, for those that could be read.
    PullRequestsFound {
        results: Vec<(
            String,
            Result<Option<crate::api::schema::WorkItemPullRequestInfo>, String>,
        )>,
        own: Vec<(String, crate::api::schema::WorkItemPullRequestInfo)>,
    },
    /// Tickets named in item titles, looked up in their trackers, as
    /// (item key, ticket key, result).
    TicketsFound {
        results: Vec<(
            String,
            String,
            Result<Option<crate::api::schema::WorkItemLinkedTicketInfo>, String>,
        )>,
    },
}

/// How often the pull requests of items' workspace branches are looked up again.
const PULL_REQUEST_LOOKUP_INTERVAL: Duration = Duration::from_secs(60);
/// How often tickets named in item titles are looked up again.
const TICKET_LOOKUP_INTERVAL: Duration = Duration::from_secs(120);

/// When a periodic background lookup runs next, and whether one is running.
#[derive(Debug)]
struct LookupSchedule {
    next: Instant,
    in_flight: bool,
    /// Asked for while one ran; the next starts as soon as that one finishes.
    requested: bool,
}

impl LookupSchedule {
    fn new(now: Instant) -> Self {
        Self {
            next: now,
            in_flight: false,
            requested: false,
        }
    }

    /// When the next one is due, unless one is running.
    fn deadline(&self) -> Option<Instant> {
        (!self.in_flight).then_some(self.next)
    }

    /// As soon as possible: now, or right after the one running.
    fn request(&mut self, now: Instant) {
        if self.in_flight {
            self.requested = true;
        } else {
            self.next = self.next.min(now);
        }
    }

    /// Whether one is due; if so, the next one after it is `interval` from now. The
    /// caller marks it in flight once it starts one.
    fn take_due(&mut self, now: Instant, interval: Duration) -> bool {
        if self.in_flight || self.next > now {
            return false;
        }
        self.next = now + interval;
        true
    }

    fn finish(&mut self, now: Instant, interval: Duration) {
        self.in_flight = false;
        self.next = if std::mem::take(&mut self.requested) {
            now
        } else {
            now + interval
        };
    }
}

/// Where an item's pull request is looked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PullRequestTarget {
    /// The branch checked out in the item's workspace.
    Workspace(String),
    /// A local branch its source knows the work is on, while the item has no workspace.
    Branch {
        repo_root: std::path::PathBuf,
        branch: String,
    },
}

/// A due pull request lookup.
pub(crate) struct PullRequestLookup {
    /// The sources to ask about branches.
    pub sources: Vec<Arc<dyn WorkItemSource>>,
    /// (item key, target) pairs whose branch's pull request is looked for.
    pub branches: Vec<(String, PullRequestTarget)>,
    /// Pull request items shown on their own, per hosting source: the item keys and, in the
    /// same order, their (repository, number).
    pub own: Vec<OwnPullRequestLookup>,
}

pub(crate) struct OwnPullRequestLookup {
    pub source: Arc<dyn WorkItemSource>,
    pub keys: Vec<String>,
    pub pulls: Vec<(String, u64)>,
}

/// The source hosting the pull request an item is, and its (repository, number).
type OwnPullRequestTarget<'a> = (&'a Arc<dyn WorkItemSource>, (String, u64));

/// A due ticket lookup: (item key, tracker, ticket key) triples.
pub(crate) type TicketLookup = Vec<(String, Arc<dyn WorkItemSource>, String)>;

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
    /// Clones linked from items, used like those mapped in the config.
    linked_clones: Vec<LinkedClone>,
    /// Repositories and tracker projects whose items stay out of this session's inbox.
    ignored_projects: Vec<WorkItemProject>,
    /// Work items moved focus to a workspace outside an API request, e.g. a new
    /// "Pick next" workspace; the server moves its shell clients there too.
    focus_requested: bool,
    /// Mapped local clones, reachable from the inbox even without an item.
    repositories: Vec<Repository>,
    /// Lookups of the pull requests of items' workspace branches.
    pull_request_lookup: LookupSchedule,
    /// Lookups of the tickets named in item titles.
    ticket_lookup: LookupSchedule,
    /// What needs you now, and since when; worked out by the app from items and agents.
    attention: attention::Tracker,
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

/// `config` with the clones linked from items added to their sources' mappings. A
/// repository the config maps keeps that mapping.
fn with_linked_clones(config: &WorkItemsConfig, linked: &[LinkedClone]) -> WorkItemsConfig {
    let mut config = config.clone();
    if let Some(github) = config.github.as_mut() {
        for clone in linked
            .iter()
            .filter(|clone| clone.source_id == github::SOURCE_ID)
        {
            if !github
                .repos
                .iter()
                .any(|repo| repo.name.eq_ignore_ascii_case(&clone.name))
            {
                github.repos.push(GithubRepoConfig {
                    name: clone.name.clone(),
                    path: clone.path.to_string_lossy().into_owned(),
                    remote: clone.remote.clone(),
                });
            }
        }
    }
    config
}

fn build_sources(config: &WorkItemsConfig) -> Vec<Arc<dyn WorkItemSource>> {
    let mut sources: Vec<Arc<dyn WorkItemSource>> = Vec::new();
    if let Some(github) = config.github.as_ref().filter(|github| github.enabled) {
        sources.push(Arc::new(github::GithubSource::new(
            github.clone(),
            config.default_agent(),
        )));
    }
    if let Some(jira) = config.jira.as_ref().filter(|jira| jira.enabled) {
        sources.push(Arc::new(jira::JiraSource::new(
            jira.clone(),
            config.default_agent(),
        )));
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
            linked_clones: Vec::new(),
            ignored_projects: Vec::new(),
            focus_requested: false,
            repositories: Vec::new(),
            pull_request_lookup: LookupSchedule::new(Instant::now()),
            ticket_lookup: LookupSchedule::new(Instant::now()),
            attention: attention::Tracker::default(),
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
        items.rebuild_sources();
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
                self.state
                    .restore_last_local_number(stored.last_local_number);
                self.owned_worktrees = stored.worktrees;
                self.pick_next = stored.pick_next;
                self.linked_clones = stored.linked_clones;
                self.ignored_projects = stored.ignored_projects;
                if !self.linked_clones.is_empty() {
                    self.rebuild_sources();
                }
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

    /// Sources and repositories for the config with the linked clones added.
    fn rebuild_sources(&mut self) {
        let config = with_linked_clones(&self.config, &self.linked_clones);
        self.sources = build_sources(&config);
        self.repositories = build_repositories(&config);
    }

    /// Remembers a clone linked from an item, replacing an earlier link of its repository.
    fn add_linked_clone(&mut self, clone: LinkedClone) {
        self.linked_clones.retain(|linked| {
            linked.source_id != clone.source_id || !linked.name.eq_ignore_ascii_case(&clone.name)
        });
        self.linked_clones.push(clone);
        self.rebuild_sources();
        self.changed();
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
        self.rebuild_sources();
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

    /// Agent of the "Pick next" workspace and the prompt that puts it in plan mode (empty
    /// for none).
    pub(crate) fn pick_next_agent(&self) -> (crate::config::AgentLaunch, &str) {
        (
            self.config.pick_next_agent(),
            self.config.pick_next_plan_command(),
        )
    }

    /// How long an agent may run one command before it counts as stuck and needs you.
    pub(crate) fn stuck_after(&self) -> Duration {
        self.config.stuck_after()
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
        // Only while something could have a pull request, or names a ticket.
        let pull_request_lookup = self.pull_request_lookup.deadline().filter(|_| {
            self.state
                .items()
                .iter()
                .any(|item| self.pull_request_target(item).is_some())
        });
        let ticket_lookup = self.ticket_lookup.deadline().filter(|_| {
            self.state
                .items()
                .iter()
                .any(|item| self.named_ticket(item).is_some())
        });
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
            .chain(pull_request_lookup)
            .chain(ticket_lookup)
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
                let result = result.map(|polled| self.without_ignored(source.as_ref(), polled));
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
                self.request_ticket_lookup_for_new_names(now);
                // A pull request item has no state to show until it is looked up.
                let hosts = self.pull_request_hosts();
                if self.state.items().iter().any(|item| {
                    item.source_id == source_id
                        && item.own_pull_request.is_none()
                        && self.own_pull_request_target(item, &hosts).is_some()
                }) {
                    self.request_pull_request_lookup(now);
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
                let (changed, review_arrived) =
                    self.state.apply_prepared(&key, &updated_at, prepared);
                self.queue_resolutions();
                // Its details may have named the branch the work is on: look it up now
                // rather than at the next interval.
                if self.state.get(&key).is_some_and(|item| {
                    item.linked_pull_request.is_none() && self.pull_request_target(item).is_some()
                }) {
                    self.request_pull_request_lookup(now);
                }
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
            WorkItemsEvent::CloneLinked { key, result } => {
                let result = result.map(|(clone, message)| {
                    self.add_linked_clone(clone);
                    message
                });
                let notices: Vec<WorkItemNotice> =
                    self.finish_action(&key, result, now).into_iter().collect();
                (!notices.is_empty(), notices)
            }
            // Handled by the app driver, which owns the response channel.
            WorkItemsEvent::TicketFetchedForAdd { .. } => (false, Vec::new()),
            WorkItemsEvent::PullRequestsFound { results, own } => {
                // Something changed while it ran, e.g. another ticket named its branch.
                self.pull_request_lookup
                    .finish(now, PULL_REQUEST_LOOKUP_INTERVAL);
                let mut changed = false;
                for (key, result) in results {
                    let found = match result {
                        Ok(found) => found,
                        // Keep what was found last; the next lookup tries again.
                        Err(error) => {
                            tracing::warn!(item = %key, %error, "pull request lookup failed");
                            continue;
                        }
                    };
                    // A workspace closed while the lookup ran may leave nothing to link.
                    let still_linkable = self
                        .state
                        .get(&key)
                        .is_some_and(|item| self.pull_request_target(item).is_some());
                    if let Some(item) = self.state.get_mut(&key) {
                        let found = found.filter(|_| still_linkable);
                        if item.linked_pull_request != found {
                            item.linked_pull_request = found;
                            changed = true;
                        }
                    }
                }
                for (key, found) in own {
                    let Some(item) = self.state.get(&key) else {
                        continue;
                    };
                    let is_it = self
                        .source(&item.source_id)
                        .and_then(|source| source.pull_request_of(item))
                        .is_some_and(|(repo, number)| {
                            repo.eq_ignore_ascii_case(&found.repo) && number == found.number
                        });
                    if let Some(item) = self.state.get_mut(&key).filter(|_| is_it) {
                        if item.own_pull_request.as_ref() != Some(&found) {
                            item.own_pull_request = Some(found);
                            changed = true;
                        }
                    }
                }
                if changed {
                    self.changed();
                }
                (changed, Vec::new())
            }
            WorkItemsEvent::TicketsFound { results } => {
                self.ticket_lookup.finish(now, TICKET_LOOKUP_INTERVAL);
                let mut changed = false;
                for (item_key, ticket_key, result) in results {
                    let found = match result {
                        Ok(found) => found,
                        // Keep what was found last; the next lookup tries again.
                        Err(error) => {
                            tracing::warn!(item = %item_key, ticket = %ticket_key, %error, "ticket lookup failed");
                            continue;
                        }
                    };
                    // The title may have changed while the lookup ran.
                    let still_named = self
                        .state
                        .get(&item_key)
                        .and_then(|item| self.named_ticket(item))
                        .is_some_and(|(_, key)| key == ticket_key);
                    if !still_named {
                        continue;
                    }
                    if let Some(item) = self.state.get_mut(&item_key) {
                        if item.linked_ticket != found {
                            item.linked_ticket = found;
                            changed = true;
                        }
                    }
                }
                if changed {
                    self.changed();
                }
                (changed, Vec::new())
            }
        }
    }

    /// Looks pull requests up again as soon as possible: now, or right after the one running.
    fn request_pull_request_lookup(&mut self, now: Instant) {
        self.pull_request_lookup.request(now);
    }

    /// Looks tickets up as soon as possible when a new or retitled item names one it is
    /// not linked to yet, rather than at the next interval.
    fn request_ticket_lookup_for_new_names(&mut self, now: Instant) {
        if self.state.items().iter().any(|item| {
            self.named_ticket(item).is_some_and(|(_, key)| {
                item.linked_ticket.as_ref().map(|ticket| &ticket.key) != Some(&key)
            })
        }) {
            self.ticket_lookup.request(now);
        }
    }

    /// When a lookup is due: the sources to ask and the items to look up, with where, plus
    /// the pull request items shown on their own. Marks the lookup in flight.
    pub(crate) fn take_due_pull_request_lookup(
        &mut self,
        now: Instant,
    ) -> Option<PullRequestLookup> {
        if !self
            .pull_request_lookup
            .take_due(now, PULL_REQUEST_LOOKUP_INTERVAL)
        {
            return None;
        }
        let branches: Vec<(String, PullRequestTarget)> = self
            .state
            .items()
            .iter()
            .filter_map(|item| Some((item.key.clone(), self.pull_request_target(item)?)))
            .collect();
        let hosts = self.pull_request_hosts();
        let mut own: Vec<OwnPullRequestLookup> = Vec::new();
        for item in self.state.items() {
            let Some((source, pull)) = self.own_pull_request_target(item, &hosts) else {
                continue;
            };
            match own
                .iter_mut()
                .find(|lookup| lookup.source.id() == source.id())
            {
                Some(lookup) => {
                    lookup.keys.push(item.key.clone());
                    lookup.pulls.push(pull);
                }
                None => own.push(OwnPullRequestLookup {
                    source: source.clone(),
                    keys: vec![item.key.clone()],
                    pulls: vec![pull],
                }),
            }
        }
        if branches.is_empty() && own.is_empty() {
            return None;
        }
        self.pull_request_lookup.in_flight = true;
        Some(PullRequestLookup {
            sources: self.sources.clone(),
            branches,
            own,
        })
    }

    /// The source hosting the pull request `item` is, and its (repository, number), when the
    /// item shows that pull request's state itself: not hidden, not "Pick next", and not
    /// folded into the item it was opened for, which shows it as its linked pull request.
    fn own_pull_request_target(
        &self,
        item: &WorkItem,
        hosts: &HashMap<(String, u64), String>,
    ) -> Option<OwnPullRequestTarget<'_>> {
        if item.is_pick_next || item.dismissed || item.snoozed_until.is_some() {
            return None;
        }
        let source = self.source(&item.source_id)?;
        let pull = source.pull_request_of(item)?;
        if self.folded_into(item, hosts).is_some() {
            return None;
        }
        Some((source, pull))
    }

    /// When a ticket lookup is due: the tickets named in item titles that are not in the
    /// inbox themselves, to ask their trackers about. Tickets in the inbox are linked from
    /// there right away, and links whose title no longer names a ticket are dropped. Marks
    /// the lookup in flight.
    pub(crate) fn take_due_ticket_lookup(&mut self, now: Instant) -> Option<TicketLookup> {
        if !self.ticket_lookup.take_due(now, TICKET_LOOKUP_INTERVAL) {
            return None;
        }
        let named: Vec<_> = self
            .state
            .items()
            .iter()
            .map(|item| {
                let named = self
                    .named_ticket(item)
                    .map(|(source, key)| (source.clone(), key));
                (item.key.clone(), named)
            })
            .collect();
        let mut lookups = Vec::new();
        let mut changed = false;
        for (item_key, named) in named {
            let linked = match named {
                None => None,
                Some((source, ticket_key)) => {
                    let in_inbox = self
                        .state
                        .get(&state::item_key(source.id(), &ticket_key))
                        .and_then(|ticket| {
                            Some(crate::api::schema::WorkItemLinkedTicketInfo {
                                source_id: ticket.source_id.clone(),
                                key: ticket.external_id.clone(),
                                url: ticket.url.clone(),
                                tracker_state: ticket.tracker_state.clone()?,
                            })
                        });
                    match in_inbox {
                        Some(linked) => Some(linked),
                        None => {
                            // Keep showing the last lookup of the same ticket until this one ends.
                            let current = self
                                .state
                                .get(&item_key)
                                .and_then(|item| item.linked_ticket.clone())
                                .filter(|linked| linked.key == ticket_key);
                            lookups.push((item_key.clone(), source, ticket_key));
                            current
                        }
                    }
                }
            };
            if let Some(item) = self.state.get_mut(&item_key) {
                if item.linked_ticket != linked {
                    item.linked_ticket = linked;
                    changed = true;
                }
            }
        }
        if changed {
            self.changed();
        }
        if lookups.is_empty() {
            return None;
        }
        self.ticket_lookup.in_flight = true;
        Some(lookups)
    }

    /// The ticket `item`'s title names, as (tracker, key), when another source tracks it. A
    /// local item has no tracker state, so its title names none.
    fn named_ticket(&self, item: &WorkItem) -> Option<(&Arc<dyn WorkItemSource>, String)> {
        if item.is_pick_next || item.is_local() {
            return None;
        }
        self.sources
            .iter()
            .filter(|source| source.id() != item.source_id)
            .find_map(|source| Some((source, source.ticket_key_in_title(&item.title)?)))
    }

    /// Where `item`'s pull request is looked for: its workspace's branch, or, without a
    /// workspace, the branch its source knows the work is on. Pull requests with their own
    /// item, hidden items and "Pick next" have none to look up.
    fn pull_request_target(&self, item: &WorkItem) -> Option<PullRequestTarget> {
        if item.is_pick_next {
            return None;
        }
        let source = self.source(&item.source_id)?;
        if source.pull_request_of(item).is_some() {
            return None;
        }
        if let Some(workspace_id) = &item.workspace_id {
            return Some(PullRequestTarget::Workspace(workspace_id.clone()));
        }
        if item.dismissed || item.resolved {
            return None;
        }
        let (repo_root, branch) = source.work_branch(item)?;
        Some(PullRequestTarget::Branch { repo_root, branch })
    }

    /// Queues workspace removal for items that just resolved, when their source wants that.
    /// A workspace shared with items still unresolved, e.g. a stack's, waits for the last one.
    fn queue_resolutions(&mut self) {
        for key in self.state.take_newly_resolved() {
            let should_remove = self
                .state
                .get(&key)
                .filter(|item| {
                    let Some(workspace_id) = item.workspace_id.as_deref() else {
                        return true;
                    };
                    !self.state.items().iter().any(|other| {
                        other.key != item.key
                            && !other.resolved
                            && other.workspace_id.as_deref() == Some(workspace_id)
                    })
                })
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

    /// Repositories and tracker projects whose items stay out of this session's inbox.
    pub(crate) fn ignored_projects(&self) -> &[WorkItemProject] {
        &self.ignored_projects
    }

    /// Keeps `project`'s items out of this session's inbox from now on. Those already in it
    /// go, unless a workspace hangs off them: work already started stays.
    pub(crate) fn ignore_project(
        &mut self,
        project: &WorkItemProject,
    ) -> Result<(), (&'static str, String)> {
        let Some(source) = self.source(&project.source_id).cloned() else {
            return Err((
                "work_item_source_not_found",
                format!("unknown work item source {}", project.source_id),
            ));
        };
        let name = project.project.trim();
        if name.is_empty() {
            return Err(("invalid_params", "project must not be empty".into()));
        }
        if self.is_ignored(source.id(), Some(name)) {
            return Ok(());
        }
        self.ignored_projects.push(WorkItemProject {
            source_id: source.id().to_string(),
            project: name.to_string(),
        });
        self.state.drop_unworked(|item| {
            item.source_id == source.id()
                && source
                    .project_of(&item.external_id)
                    .is_some_and(|of| of.eq_ignore_ascii_case(name))
        });
        self.changed();
        Ok(())
    }

    /// Lets `project`'s items into this session's inbox again; its source is polled at once
    /// so they come back without waiting for the next round.
    pub(crate) fn unignore_project(&mut self, project: &WorkItemProject) {
        let name = project.project.trim();
        let before = self.ignored_projects.len();
        self.ignored_projects.retain(|ignored| {
            ignored.source_id != project.source_id || !ignored.project.eq_ignore_ascii_case(name)
        });
        if self.ignored_projects.len() == before {
            return;
        }
        if self.source(&project.source_id).is_some() {
            self.next_poll
                .insert(project.source_id.clone(), Instant::now());
        }
        self.changed();
    }

    /// Whether `project` of `source_id` is kept out of this session's inbox.
    fn is_ignored(&self, source_id: &str, project: Option<&str>) -> bool {
        project.is_some_and(|project| {
            self.ignored_projects.iter().any(|ignored| {
                ignored.source_id == source_id && ignored.project.eq_ignore_ascii_case(project)
            })
        })
    }

    /// `polled` without the items of ignored projects, except those already in the inbox
    /// with a workspace: work already started is still followed to its end.
    fn without_ignored(
        &self,
        source: &dyn WorkItemSource,
        mut polled: Vec<SourceItem>,
    ) -> Vec<SourceItem> {
        if self.ignored_projects.is_empty() {
            return polled;
        }
        polled.retain(|item| {
            !self.is_ignored(source.id(), source.project_of(&item.external_id).as_deref())
                || self
                    .state
                    .get(&state::item_key(source.id(), &item.external_id))
                    .is_some_and(|existing| existing.workspace_id.is_some())
        });
        polled
    }

    /// Marks a `Perform` choice as running: the item shows a spinner until the source stops
    /// reporting it. Fails while one is already running.
    pub(crate) fn begin_action(&mut self, key: &str, choice_id: &str) -> Result<(), &'static str> {
        let item = self.state.get_mut(key).ok_or("work_item_not_found")?;
        if item.action_running.is_some() {
            return Err("work_item_busy");
        }
        item.action_running = Some(choice_id.to_string());
        item.action_outcome = None;
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
        let choice_id = item.action_running.take()?;
        let before = item.phase_before_action.take();
        let context = item.context.clone();
        let source_id = item.source_id.clone();
        let notice = match result {
            Ok(message) => {
                // Work in its own workspace carries on (e.g. after updating the tracker), and
                // so does a ticket whose pull request item took the choice: that item leaves
                // once its source agrees (e.g. a merge), the ticket does not. A linked clone
                // leaves the item to be worked on locally. Anything else waits for the source
                // to drop the item.
                let carried = source::carried_original_id(&choice_id).map(str::to_string);
                if before == Some(WorkItemPhase::Local)
                    || carried.is_some()
                    || choice_id == source::LINK_CLONE_CHOICE_ID
                {
                    item.phase = before.unwrap_or(WorkItemPhase::Pending);
                }
                item.action_outcome = Some(WorkItemActionOutcome {
                    choice_id,
                    succeeded: true,
                    message: message.clone(),
                });
                self.next_poll.insert(source_id, now);
                // The pull request item that took the choice is polled at once too, so it
                // leaves the inbox as soon as its source agrees.
                let folded_source = carried.and_then(|original_id| {
                    self.folded_choice(key, &original_id)
                        .map(|(source, _)| source.id().to_string())
                });
                if let Some(folded_source) = folded_source {
                    self.next_poll.insert(folded_source, now);
                }
                // A change may be visible on the linked pull request too, e.g. ready for review.
                self.request_pull_request_lookup(now);
                WorkItemNotice {
                    title: message,
                    body: Some(context),
                }
            }
            Err(error) => {
                item.phase = before.unwrap_or(WorkItemPhase::Pending);
                item.action_error = Some(error.clone());
                item.action_outcome = Some(WorkItemActionOutcome {
                    choice_id,
                    succeeded: false,
                    message: error.clone(),
                });
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
            self.request_pull_request_lookup(Instant::now());
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
            self.request_ticket_lookup_for_new_names(Instant::now());
            self.changed();
        }
        item
    }

    /// Makes a local item: work named by hand, with no tracker behind it. With `workspace_id`
    /// the work goes on in that workspace, taken from any other item that has it. Returns the
    /// new item's key.
    pub(crate) fn create_local(&mut self, title: &str, workspace_id: Option<&str>) -> String {
        let key = self
            .state
            .create_local(title, workspace_id, rfc3339_utc(unix_now()));
        self.changed();
        key
    }

    /// Marks a local item done, or not done after all.
    pub(crate) fn set_local_resolved(&mut self, key: &str, resolved: bool) -> Result<(), NotFound> {
        if self.state.set_local_resolved(key, resolved)? {
            self.changed();
        }
        Ok(())
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
        let hosts = self.pull_request_hosts();
        self.state.get(key).map(|item| self.project(item, &hosts))
    }

    fn project(&self, item: &WorkItem, hosts: &HashMap<(String, u64), String>) -> WorkItemInfo {
        let (choices, reminder) = self.choices_and_reminder(item, hosts);
        let project = (!item.is_pick_next)
            .then(|| self.source(&item.source_id))
            .flatten()
            .and_then(|source| source.project_of(&item.external_id));
        WorkItemInfo {
            project,
            ..item.info(
                choices,
                reminder.map(|reminder| reminder.message),
                self.folded_into(item, hosts),
                self.attention
                    .get(&attention::Subject::Item(item.key.clone()))
                    .cloned(),
            )
        }
    }

    /// The item whose linked pull request `item` is, if another item shows it.
    fn folded_into(
        &self,
        item: &WorkItem,
        hosts: &HashMap<(String, u64), String>,
    ) -> Option<String> {
        self.source(&item.source_id)
            .and_then(|source| source.pull_request_of(item))
            .and_then(|(repo, number)| hosts.get(&(repo.to_ascii_lowercase(), number)))
            .filter(|host| **host != item.key)
            .cloned()
    }

    /// Items showing a linked pull request, by (lowercased repository, number). Hidden
    /// items host nothing, so the pull request's own item stays reachable.
    fn pull_request_hosts(&self) -> HashMap<(String, u64), String> {
        self.state
            .items()
            .iter()
            .filter(|item| !item.dismissed && item.snoozed_until.is_none())
            .filter_map(|item| {
                let pull_request = item.linked_pull_request.as_ref()?;
                Some((
                    (pull_request.repo.to_ascii_lowercase(), pull_request.number),
                    item.key.clone(),
                ))
            })
            .collect()
    }

    /// The choices offered for `item`, including the start reminder's and those carried from
    /// the pull request item folded into it.
    pub(crate) fn item_choices(&self, item: &WorkItem) -> source::ItemChoices {
        self.choices_and_reminder(item, &self.pull_request_hosts())
            .0
    }

    /// The source's choices, plus, while the item has a workspace and its tracker lags
    /// behind, the reminder's fix (the default when available) and a way to mute it. A linked
    /// pull request adds what its own inbox item offers that the item can carry out for it,
    /// then a way to open it.
    /// A local item has no source: it offers Herdr's own choice to mark it done or not.
    fn choices_and_reminder(
        &self,
        item: &WorkItem,
        hosts: &HashMap<(String, u64), String>,
    ) -> (source::ItemChoices, Option<source::StartReminder>) {
        if item.is_local() {
            return (source::local_choices(item.resolved), None);
        }
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
        // A stack the item's pull request is part of may offer a choice covering all of it.
        if let Some((repo, stack_number)) = item
            .own_pull_request
            .as_ref()
            .and_then(|pull| Some((&pull.repo, pull.stack.as_ref()?.number)))
        {
            let members: Vec<&WorkItem> = self
                .state
                .items()
                .iter()
                .filter(|other| other.key != item.key && other.source_id == item.source_id)
                .filter(|other| {
                    other.own_pull_request.as_ref().is_some_and(|pull| {
                        pull.repo.eq_ignore_ascii_case(repo)
                            && pull.stack.as_ref().map(|stack| stack.number) == Some(stack_number)
                    })
                })
                .collect();
            choices.choices.extend(source.stack_choice(item, &members));
        }
        let close = self.close_ticket(item);
        // Once the work has landed there is no work left to start.
        let reminder =
            (close.is_none() && item.workspace_id.is_some() && !item.start_reminder_muted)
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
                    options: Vec::new(),
                    agent: None,
                },
            );
        }
        if let Some(pull_request) = &item.linked_pull_request {
            let carried = self.carried_choices(item, hosts);
            // After the tracker fix and its mute, when the reminder shows.
            let mut at = choices
                .choices
                .len()
                .min(usize::from(reminder.is_some()) * 2);
            // What asks for the default, strongest first: the pull request item's own default,
            // the tracker fix, a draft waiting on you. Without any, it is the pull request.
            let mut default_chosen = reminder.as_ref().is_some_and(|reminder| {
                choices.default_choice_id.as_deref() == Some(reminder.choice.choice_id.as_str())
            });
            if pull_request.is_draft {
                // Waiting on you: the default, unless the tracker fix already is.
                if !default_chosen {
                    choices.default_choice_id = Some(source::PULL_REQUEST_READY_CHOICE_ID.into());
                }
                default_chosen = true;
                choices.choices.insert(
                    at,
                    crate::api::schema::WorkItemChoiceInfo {
                        choice_id: source::PULL_REQUEST_READY_CHOICE_ID.into(),
                        label: "Mark pull request ready for review".into(),
                        description: Some(format!(
                            "{}#{} is a draft; reviewers are asked once it is ready",
                            pull_request.repo, pull_request.number
                        )),
                        action: crate::api::schema::WorkItemChoiceAction::Perform,
                        disabled_reason: None,
                        confirm: None,
                        options: Vec::new(),
                        agent: None,
                    },
                );
                at += 1;
            }
            if let Some(default) = carried
                .iter()
                .find(|carried| carried.is_default && carried.choice.disabled_reason.is_none())
            {
                choices.default_choice_id = Some(default.choice.choice_id.clone());
                default_chosen = true;
            }
            for carried in carried {
                choices.choices.insert(at, carried.choice);
                at += 1;
            }
            choices.choices.insert(
                at,
                crate::api::schema::WorkItemChoiceInfo {
                    choice_id: source::PULL_REQUEST_OPEN_CHOICE_ID.into(),
                    label: "Open pull request".into(),
                    description: Some(format!(
                        "{}#{} · {}",
                        pull_request.repo, pull_request.number, pull_request.status
                    )),
                    action: crate::api::schema::WorkItemChoiceAction::OpenUrl {
                        url: pull_request.url.clone(),
                    },
                    disabled_reason: None,
                    confirm: None,
                    options: Vec::new(),
                    agent: None,
                },
            );
            if !default_chosen && item.open_pull_request().is_some() {
                // The work is under review, which takes the start-work default away.
                choices.default_choice_id = Some(source::PULL_REQUEST_OPEN_CHOICE_ID.into());
            }
        }
        if let Some(close) = close {
            // Closing the ticket is what is left to do, ahead of starting more work.
            if close.choice.disabled_reason.is_none() {
                choices.default_choice_id = Some(close.choice.choice_id.clone());
            }
            choices.choices.insert(0, close.choice);
        }
        (choices, reminder)
    }

    /// The choices of the pull request items folded into `host` that it can offer in their
    /// place: those their source carries out and those that brief an agent. Starting a
    /// workspace for a pull request item, or opening it, stays with that item. Only while the
    /// pull request is in flight, the most urgent item's choices first and each choice once.
    fn carried_choices(
        &self,
        host: &WorkItem,
        hosts: &HashMap<(String, u64), String>,
    ) -> Vec<CarriedChoice<'_>> {
        use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo};

        if host.open_pull_request().is_none() {
            return Vec::new();
        }
        let mut folded: Vec<(&WorkItem, &Arc<dyn WorkItemSource>)> = self
            .state
            .items()
            .iter()
            .filter(|item| !item.dismissed && item.snoozed_until.is_none() && !item.resolved)
            .filter(|item| self.folded_into(item, hosts).as_deref() == Some(host.key.as_str()))
            .filter_map(|item| Some((item, self.source(&item.source_id)?)))
            .collect();
        // Stable: items that ask equally much stay in the order the inbox lists them.
        folded.sort_by_cached_key(|(item, _)| {
            self.tracker_need(item)
                .map_or((true, AttentionKind::Unknown), |need| (false, need.kind))
        });
        let mut seen = HashSet::new();
        let mut carried = Vec::new();
        for (item, source) in folded {
            let offered = source.choices(item);
            for choice in offered.choices {
                let carries_out = matches!(
                    choice.action,
                    WorkItemChoiceAction::Perform | WorkItemChoiceAction::BriefAgent
                );
                if !carries_out || !seen.insert(choice.choice_id.clone()) {
                    continue;
                }
                let disabled_reason = if choice.action == WorkItemChoiceAction::BriefAgent {
                    // The agent that gets the brief is the ticket's, not the pull request item's.
                    host.workspace_id
                        .is_none()
                        .then(|| "Work on it locally first".to_string())
                } else {
                    choice.disabled_reason.clone()
                };
                carried.push(CarriedChoice {
                    item,
                    source,
                    original_id: choice.choice_id.clone(),
                    is_default: offered.default_choice_id.as_deref()
                        == Some(choice.choice_id.as_str()),
                    choice: WorkItemChoiceInfo {
                        choice_id: source::carried_choice_id(&choice.choice_id),
                        disabled_reason,
                        ..choice
                    },
                });
            }
        }
        carried
    }

    /// The pull request item folded into `host_key` that offers `original_id`, the choice the
    /// ticket carries as `pull_request:` and that id, with its source. `None` once the item no
    /// longer offers it, or the pull request is no longer in flight.
    pub(crate) fn folded_choice(
        &self,
        host_key: &str,
        original_id: &str,
    ) -> Option<(&Arc<dyn WorkItemSource>, &WorkItem)> {
        let host = self.state.get(host_key)?;
        self.carried_choices(host, &self.pull_request_hosts())
            .into_iter()
            .find(|carried| carried.original_id == original_id)
            .map(|carried| (carried.source, carried.item))
    }

    /// How to close `item`'s ticket, while its linked pull request is merged and the
    /// tracker still has the ticket open.
    fn close_ticket(&self, item: &WorkItem) -> Option<source::CloseTicket> {
        let pull_request = item
            .linked_pull_request
            .as_ref()
            .filter(|pull_request| pull_request.status == "merged")?;
        if item.resolved {
            return None;
        }
        self.source(&item.source_id)?
            .close_ticket(item, pull_request)
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
            // Its link went with the workspace; the branch it is on may still find it.
            self.request_pull_request_lookup(Instant::now());
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

    /// Whether `key` is being provisioned, on its own or as part of another item's workspace.
    pub(crate) fn has_job(&self, key: &str) -> bool {
        self.jobs.contains_key(key)
            || self
                .jobs
                .values()
                .any(|job| job.plan.shared_with.iter().any(|shared| shared == key))
    }

    /// Starts provisioning `key`; returns the new job id.
    pub(crate) fn start_job(&mut self, key: &str, plan: ProvisionPlan) -> Result<u64, NotFound> {
        let item = self.state.get_mut(key).ok_or(NotFound)?;
        item.phase = WorkItemPhase::Local;
        item.seen = true;
        item.provisioning = Some(provision::initial_progress(&plan));
        item.brief_failed_at = None;
        item.brief_failure_outdated = false;
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

    /// Applies `update` to the progress of the job's item, and notes when its agent brief
    /// fails.
    pub(crate) fn update_progress(
        &mut self,
        job_id: u64,
        update: impl FnOnce(&mut WorkItemProvisioningInfo),
    ) {
        let Some(key) = self.job(job_id).map(|job| job.key.clone()) else {
            return;
        };
        let Some(item) = self.state.get_mut(&key) else {
            return;
        };
        let Some(progress) = item.provisioning.as_mut() else {
            return;
        };
        let before = progress.clone();
        update(progress);
        let changed = *progress != before;
        if provision::has_failed_brief(progress) && !provision::has_failed_brief(&before) {
            // Compared with agents' turn ends, see `settle_brief_failures`.
            item.brief_failed_at = Some(Instant::now());
        }
        if changed {
            self.changed();
        }
    }

    pub(crate) fn progress(&self, job_id: u64) -> Option<&WorkItemProvisioningInfo> {
        let job = self.job(job_id)?;
        self.state.get(&job.key)?.provisioning.as_ref()
    }

    /// Records the provisioned workspace on the job, its item and the items sharing it that
    /// have none yet.
    pub(crate) fn link_workspace(&mut self, job_id: u64, workspace_id: &str) {
        let Some(job) = self.job_mut(job_id) else {
            return;
        };
        job.workspace_id = Some(workspace_id.to_string());
        let key = job.key.clone();
        let shared_with = job.plan.shared_with.clone();
        let Some(item) = self.state.get_mut(&key) else {
            return;
        };
        item.workspace_id = Some(workspace_id.to_string());
        for shared in shared_with {
            if let Some(item) = self
                .state
                .get_mut(&shared)
                .filter(|item| item.workspace_id.is_none())
            {
                item.workspace_id = Some(workspace_id.to_string());
                item.phase = WorkItemPhase::Local;
                item.seen = true;
            }
        }
        self.request_pull_request_lookup(Instant::now());
        self.changed();
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
        let hosts = self.pull_request_hosts();
        self.state
            .sorted()
            .into_iter()
            .map(|item| self.project(item, &hosts))
            .collect()
    }

    /// Workspaces that belong to an item; agents anywhere else are reported on their own.
    pub(crate) fn item_workspace_ids(&self) -> HashSet<&str> {
        self.state
            .items()
            .iter()
            .filter_map(|item| item.workspace_id.as_deref())
            .collect()
    }

    pub(crate) fn attention(&self, subject: &attention::Subject) -> Option<&AttentionInfo> {
        self.attention.get(subject)
    }

    /// Forgets which needs were already reported; the next update records without events.
    pub(crate) fn reset_attention(&mut self) {
        self.attention.reset();
    }

    /// Works out what needs you from each item's own state and `agents`, the folded verdict
    /// of each workspace's agents, plus `loose`, the needs of agents outside every item keyed
    /// by pane. Items folded into another report through it. Bumps the revision only when
    /// an item's attention changes.
    pub(crate) fn update_attention(
        &mut self,
        agents: &HashMap<String, attention::AgentVerdict>,
        loose: Vec<(String, attention::Candidate)>,
        now_unix: u64,
    ) -> Vec<attention::Transition> {
        use attention::{Candidate, ItemSignals, Subject};

        self.settle_brief_failures(agents);
        let hosts = self.pull_request_hosts();
        let mut folded: HashMap<String, Option<attention::Need>> = HashMap::new();
        for item in self.state.items() {
            if let Some(host) = self.folded_into(item, &hosts) {
                let slot = folded.entry(host).or_default();
                *slot = attention::most_urgent(slot.take(), self.tracker_need(item));
            }
        }
        let idle = attention::AgentVerdict::default();
        let mut current: HashMap<Subject, Candidate> = loose
            .into_iter()
            .map(|(pane_id, candidate)| (Subject::Pane(pane_id), candidate))
            .collect();
        // A workspace shared by several items, e.g. a stack's, reports its agents through one
        // of them, preferably one still unresolved, so its needs are not counted once per item.
        let mut reporters: HashMap<&str, &WorkItem> = HashMap::new();
        for item in self.state.items() {
            let Some(workspace_id) = item.workspace_id.as_deref() else {
                continue;
            };
            let reporter = reporters.entry(workspace_id).or_insert(item);
            if reporter.resolved && !item.resolved {
                *reporter = item;
            }
        }
        for item in self.state.items() {
            if self.folded_into(item, &hosts).is_some() {
                continue;
            }
            // Done is done: a local item marked so needs nothing, whatever its agents do.
            if item.is_local() && item.resolved {
                continue;
            }
            let verdict = item
                .workspace_id
                .as_deref()
                .filter(|workspace_id| {
                    reporters
                        .get(workspace_id)
                        .is_some_and(|reporter| reporter.key == item.key)
                })
                .and_then(|workspace_id| agents.get(workspace_id))
                .unwrap_or(&idle);
            let signals = ItemSignals {
                failure: failure_need(item),
                busy: self.jobs.contains_key(&item.key) || item.action_running.is_some(),
                tracker: attention::most_urgent(
                    self.tracker_need(item),
                    folded.remove(&item.key).flatten(),
                ),
            };
            if let Some(need) = attention::item_need(verdict, signals) {
                current.insert(
                    Subject::Item(item.key.clone()),
                    Candidate {
                        need,
                        title: item.title.clone(),
                        workspace_id: item.workspace_id.clone(),
                    },
                );
            }
        }
        let update = self.attention.update(current, now_unix);
        if update.items_changed {
            // Not persisted: attention is worked out again after a restart.
            self.revision += 1;
        }
        update.transitions
    }

    /// A failed agent brief stops needing you once an agent in the item's workspace has taken
    /// a turn after the failure: it worked, so it became ready after all, and the brief was
    /// sent late or by hand. This holds for good, so the failure does not return once that
    /// agent goes away. The step stays failed in `provisioning`; only the attention clears.
    /// The failure and the turn are both moments on the monotonic clock, so a machine that
    /// slept in between cannot reorder them.
    fn settle_brief_failures(&mut self, agents: &HashMap<String, attention::AgentVerdict>) {
        for item in self.state.items_mut() {
            if item.brief_failure_outdated {
                continue;
            }
            let Some(failed_at) = item.brief_failed_at else {
                continue;
            };
            let Some(verdict) = item
                .workspace_id
                .as_deref()
                .and_then(|workspace_id| agents.get(workspace_id))
            else {
                continue;
            };
            let brief_failed = item.provisioning.as_ref().is_some_and(|provisioning| {
                provisioning.finished && provision::has_failed_brief(provisioning)
            });
            item.brief_failure_outdated = brief_failed && verdict.took_turn_after(failed_at);
        }
    }

    /// What the tracker asks of you, unless the item is hidden, waits on others, or was
    /// handled outside Herdr. Newly arrived items stop asking once a choice is made.
    fn tracker_need(&self, item: &WorkItem) -> Option<attention::Need> {
        if item.is_pick_next
            || item.dismissed
            || item.snoozed_until.is_some()
            || item.resolved
            || item.waiting
            || item.phase == WorkItemPhase::AwaitingExternal
        {
            return None;
        }
        if let Some(close) = self.close_ticket(item) {
            return Some(attention::Need::new(
                AttentionKind::ReadyToClose,
                close.reason,
            ));
        }
        let need = self.source(&item.source_id)?.tracker_need(item)?;
        (need.kind != AttentionKind::New || item.phase == WorkItemPhase::Pending).then_some(need)
    }

    /// The linked pull request of `key`, with the source hosting it.
    pub(crate) fn linked_pull_request(
        &self,
        key: &str,
    ) -> Option<(
        Arc<dyn WorkItemSource>,
        crate::api::schema::WorkItemPullRequestInfo,
    )> {
        let pull_request = self.state.get(key)?.linked_pull_request.clone()?;
        let source = self.source(&pull_request.source_id)?.clone();
        Some((source, pull_request))
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
            store.save(
                self.state.items(),
                &self.owned_worktrees,
                &self.pick_next,
                &self.linked_clones,
                &self.ignored_projects,
                self.state.last_local_number(),
            );
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

/// A choice of a pull request item folded into a ticket, as the ticket offers it.
struct CarriedChoice<'a> {
    /// The folded item that offers it.
    item: &'a WorkItem,
    source: &'a Arc<dyn WorkItemSource>,
    /// The folded item's own id for it.
    original_id: String,
    /// As the ticket offers it: under a prefixed id, and a brief waits on the ticket's own
    /// workspace rather than the pull request item's.
    choice: crate::api::schema::WorkItemChoiceInfo,
    /// Whether the folded item's source makes it that item's default.
    is_default: bool,
}

/// A failed choice, a failed provisioning step, or a workspace that could not be removed. A
/// failed agent brief counts only until an agent took a turn after it, see
/// `WorkItems::settle_brief_failures`.
fn failure_need(item: &WorkItem) -> Option<attention::Need> {
    let failed = |reason: String| Some(attention::Need::new(AttentionKind::Failed, reason));
    if let Some(outcome) = item
        .action_outcome
        .as_ref()
        .filter(|outcome| !outcome.succeeded)
    {
        return failed(outcome.message.clone());
    }
    let failed_step = item
        .provisioning
        .as_ref()
        .filter(|provisioning| provisioning.finished)
        .and_then(|provisioning| {
            provisioning.steps.iter().find(|step| {
                step.status == crate::api::schema::WorkItemStepStatus::Failed
                    && !(item.brief_failure_outdated
                        && step.step == crate::api::schema::WorkItemStep::AgentBrief)
            })
        });
    if let Some(step) = failed_step {
        return failed(
            step.detail
                .clone()
                .unwrap_or_else(|| format!("{} failed", step.label)),
        );
    }
    item.resolve_error.clone().and_then(failed)
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

    #[test]
    fn a_lookup_asked_for_while_one_runs_follows_it_instead_of_waiting_the_interval() {
        let source = FakeSource::with_items(Vec::new());
        *source.work_branch.lock().unwrap() = Some("ar/work".into());
        let mut items = WorkItems::for_test(vec![source], Instant::now());
        poll(&mut items, &["a"]);
        let now = Instant::now();
        assert!(items.take_due_pull_request_lookup(now).is_some());

        // A second ticket's details name its branch while the first lookup runs.
        poll(&mut items, &["a", "b"]);
        items.apply_event(
            WorkItemsEvent::Prepared {
                key: "fake:b".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                prepared: PreparedItem {
                    waiting: false,
                    detail: None,
                    summary: None,
                    error: None,
                    done: false,
                },
            },
            now,
        );
        items.apply_event(
            WorkItemsEvent::PullRequestsFound {
                results: vec![("fake:a".into(), Ok(None))],
                own: Vec::new(),
            },
            now,
        );

        let lookup = items
            .take_due_pull_request_lookup(now)
            .expect("the asked-for lookup starts at once");
        assert!(lookup.branches.iter().any(|(key, _)| key == "fake:b"));
    }

    #[test]
    fn a_pull_request_item_shows_its_own_state_once_looked_up() {
        let source = FakeSource::with_items(Vec::new());
        *source.pull_request_prefix.lock().unwrap() = Some("pr:".into());
        *source.pull_request_status.lock().unwrap() = Some("approved · CI failing".into());
        let mut items = WorkItems::for_test(vec![source], Instant::now());
        poll(&mut items, &["pr:5", "ticket"]);
        let now = Instant::now();

        let lookup = items
            .take_due_pull_request_lookup(now)
            .expect("a pull request item has a state to look up");
        let [own] = lookup.own.as_slice() else {
            panic!("one source hosts the pull request");
        };
        assert_eq!(own.keys, vec!["fake:pr:5".to_string()]);
        assert_eq!(own.pulls, vec![("o/r".to_string(), 5)]);
        let found = own.source.pull_request_statuses(&own.pulls).unwrap();
        let own = own
            .keys
            .iter()
            .cloned()
            .zip(found)
            .filter_map(|(key, status)| Some((key, status?)))
            .collect();
        items.apply_event(
            WorkItemsEvent::PullRequestsFound {
                results: Vec::new(),
                own,
            },
            now,
        );

        let pull = items.item_info("fake:pr:5").expect("item");
        assert_eq!(
            pull.own_pull_request.map(|pull| pull.status).as_deref(),
            Some("approved · CI failing")
        );
    }

    #[test]
    fn a_pull_request_item_without_a_state_to_show_keeps_the_lookup_on_its_interval() {
        let source = FakeSource::with_items(Vec::new());
        *source.pull_request_prefix.lock().unwrap() = Some("pr:".into());
        // A ticket whose branch is looked up gives every round something to do.
        *source.work_branch.lock().unwrap() = Some("ar/work".into());
        let mut items = WorkItems::for_test(vec![source], Instant::now());
        poll(&mut items, &["pr:5", "ticket"]);
        items.hide("fake:pr:5", None).expect("item");
        let now = Instant::now();
        // The round due at start runs and finishes, scheduling the next a minute out.
        assert!(items.take_due_pull_request_lookup(now).is_some());
        items.apply_event(
            WorkItemsEvent::PullRequestsFound {
                results: Vec::new(),
                own: Vec::new(),
            },
            now,
        );

        // Hidden, it is never looked up, so it must not pull the next round forward.
        poll(&mut items, &["pr:5", "ticket"]);
        assert!(items.take_due_pull_request_lookup(Instant::now()).is_none());
    }

    fn poll_source(items: &mut WorkItems, source_id: &str, result: Vec<SourceItem>) {
        items.apply_event(
            WorkItemsEvent::Polled {
                source_id: source_id.into(),
                result: Ok(result),
            },
            Instant::now(),
        );
    }

    fn titled(id: &str, title: &str) -> SourceItem {
        SourceItem {
            title: title.into(),
            ..source_item(id)
        }
    }

    fn with_tracker() -> WorkItems {
        WorkItems::for_test(
            vec![
                FakeSource::with_items(Vec::new()),
                FakeSource::tracker("tracker", "T-"),
            ],
            Instant::now(),
        )
    }

    fn linked(key: &str) -> crate::api::schema::WorkItemLinkedTicketInfo {
        crate::api::schema::WorkItemLinkedTicketInfo {
            source_id: "tracker".into(),
            key: key.into(),
            url: format!("https://example.test/{key}"),
            tracker_state: "In Progress · Ada".into(),
        }
    }

    fn linked_ticket(items: &WorkItems, key: &str) -> Option<String> {
        items
            .item_info(key)
            .and_then(|info| info.linked_ticket)
            .map(|ticket| format!("{} {} {}", ticket.key, ticket.url, ticket.tracker_state))
    }

    #[test]
    fn a_title_naming_a_ticket_in_the_inbox_links_it_without_asking_its_tracker() {
        let mut items = with_tracker();
        let ticket = SourceItem {
            tracker_state: Some("In Progress · Ada".into()),
            ..source_item("T-1")
        };
        poll_source(&mut items, "tracker", vec![ticket]);
        poll_source(&mut items, "fake", vec![titled("pr", "T-1 Fix login")]);

        assert!(items.take_due_ticket_lookup(Instant::now()).is_none());
        assert_eq!(
            linked_ticket(&items, "fake:pr").as_deref(),
            Some("T-1 https://example.test/T-1 In Progress · Ada")
        );
    }

    #[test]
    fn a_title_naming_a_ticket_outside_the_inbox_links_it_once_its_tracker_answers() {
        let mut items = with_tracker();
        poll_source(&mut items, "fake", vec![titled("pr", "T-2 Fix login")]);
        let now = Instant::now();

        let lookups = items.take_due_ticket_lookup(now).expect("lookup due");
        let asked: Vec<(&str, &str, &str)> = lookups
            .iter()
            .map(|(item, source, key)| (item.as_str(), source.id(), key.as_str()))
            .collect();
        assert_eq!(asked, [("fake:pr", "tracker", "T-2")]);
        items.apply_event(
            WorkItemsEvent::TicketsFound {
                results: vec![("fake:pr".into(), "T-2".into(), Ok(Some(linked("T-2"))))],
            },
            now,
        );
        assert_eq!(
            linked_ticket(&items, "fake:pr").as_deref(),
            Some("T-2 https://example.test/T-2 In Progress · Ada")
        );
    }

    #[test]
    fn an_answer_for_a_ticket_the_title_stopped_naming_while_it_ran_is_dropped() {
        let mut items = with_tracker();
        poll_source(&mut items, "fake", vec![titled("pr", "T-2 Fix login")]);
        let now = Instant::now();
        items.take_due_ticket_lookup(now).expect("lookup due");

        poll_source(&mut items, "fake", vec![titled("pr", "Fix login")]);
        items.apply_event(
            WorkItemsEvent::TicketsFound {
                results: vec![("fake:pr".into(), "T-2".into(), Ok(Some(linked("T-2"))))],
            },
            now,
        );
        assert_eq!(linked_ticket(&items, "fake:pr"), None);
    }

    #[test]
    fn a_retitled_item_loses_its_link_at_the_next_lookup() {
        let mut items = with_tracker();
        poll_source(&mut items, "fake", vec![titled("pr", "T-2 Fix login")]);
        let now = Instant::now();
        items.take_due_ticket_lookup(now).expect("lookup due");
        items.apply_event(
            WorkItemsEvent::TicketsFound {
                results: vec![("fake:pr".into(), "T-2".into(), Ok(Some(linked("T-2"))))],
            },
            now,
        );

        poll_source(&mut items, "fake", vec![titled("pr", "Fix login")]);
        let later = now + TICKET_LOOKUP_INTERVAL;
        assert!(items.take_due_ticket_lookup(later).is_none());
        assert_eq!(linked_ticket(&items, "fake:pr"), None);
    }

    const TICKET: &str = "tracker:T-1";

    fn pull_request(status: &str) -> crate::api::schema::WorkItemPullRequestInfo {
        crate::api::schema::WorkItemPullRequestInfo {
            source_id: "fake".into(),
            repo: "o/r".into(),
            number: 5,
            url: "https://example.test/o/r/pull/5".into(),
            is_draft: status == "draft",
            status: status.into(),
            stack: None,
        }
    }

    /// A ticket of the `tracker` source with the pull request `o/r#5` linked, and that pull
    /// request's own inbox item, `pr:5` of the `fake` source, folded into it. The item offers
    /// `do` by default, and its `brief` is disabled the way it is while the item has no
    /// workspace of its own.
    fn ticket_with_pull_request(status: &str) -> (WorkItems, Arc<FakeSource>, Arc<FakeSource>) {
        let pulls = FakeSource::with_items(Vec::new());
        *pulls.pull_request_prefix.lock().unwrap() = Some("pr:".into());
        *pulls.default_choice.lock().unwrap() = Some("do".into());
        *pulls.brief_disabled.lock().unwrap() = Some("Work on it locally first".into());
        let tracker = FakeSource::tracker("tracker", "T-");
        *tracker.work_branch.lock().unwrap() = Some("ar/t-1".into());
        let mut items = WorkItems::for_test(vec![pulls.clone(), tracker.clone()], Instant::now());
        poll_source(&mut items, "fake", vec![source_item("pr:5")]);
        poll_source(&mut items, "tracker", vec![source_item("T-1")]);
        items.apply_event(
            WorkItemsEvent::PullRequestsFound {
                results: vec![(TICKET.into(), Ok(Some(pull_request(status))))],
                own: Vec::new(),
            },
            Instant::now(),
        );
        (items, pulls, tracker)
    }

    fn choice_ids(items: &WorkItems, key: &str) -> Vec<String> {
        items
            .item_info(key)
            .expect("item")
            .choices
            .into_iter()
            .map(|choice| choice.choice_id)
            .collect()
    }

    #[test]
    fn a_ticket_offers_what_its_pull_request_item_can_carry_out_and_defaults_to_its_default() {
        let (items, _, _) = ticket_with_pull_request("approved");

        // The pull request item's workspace and browser choices stay with it; what the source
        // carries out or hands to an agent comes first, then opening the pull request, then
        // the ticket's own.
        assert_eq!(
            choice_ids(&items, TICKET),
            [
                "pull_request:do",
                "pull_request:brief",
                "pull_request_open",
                "local",
                "web",
                "do",
                "brief"
            ]
        );
        let ticket = items.item_info(TICKET).expect("ticket");
        assert_eq!(ticket.default_choice_id.as_deref(), Some("pull_request:do"));
        // A merge still asks twice.
        assert_eq!(ticket.choices[0].confirm.as_deref(), Some("Sure?"));
    }

    #[test]
    fn a_carried_brief_waits_for_the_tickets_own_workspace_not_the_pull_request_items() {
        let (mut items, pulls, _) = ticket_with_pull_request("approved");
        let carried = |items: &WorkItems| {
            items
                .item_info(TICKET)
                .expect("ticket")
                .choices
                .into_iter()
                .find(|choice| choice.choice_id == "pull_request:brief")
                .expect("carried brief")
                .disabled_reason
        };

        // The pull request item would brief its agent, but the agent is the ticket's.
        *pulls.brief_disabled.lock().unwrap() = None;
        assert_eq!(carried(&items).as_deref(), Some("Work on it locally first"));

        // The pull request item has no workspace to brief, the ticket has.
        *pulls.brief_disabled.lock().unwrap() = Some("Work on it locally first".into());
        items.link(TICKET, "w1").expect("ticket");
        assert_eq!(carried(&items), None);
    }

    #[test]
    fn choices_come_from_the_pull_request_item_that_asks_most_and_each_only_once() {
        let (mut items, _, _) = ticket_with_pull_request("approved");
        // A second inbox item for the same pull request, listed after the first, which waits
        // on others and so asks nothing of you.
        poll_source(
            &mut items,
            "fake",
            vec![source_item("pr:5"), source_item("pr:05")],
        );
        items.state.get_mut("fake:pr:5").expect("item").waiting = true;

        let ids = choice_ids(&items, TICKET);
        assert_eq!(
            ids.iter().filter(|id| *id == "pull_request:do").count(),
            1,
            "{ids:?}"
        );
        let (source, item) = items.folded_choice(TICKET, "do").expect("carried");
        assert_eq!((source.id(), item.key.as_str()), ("fake", "fake:pr:05"));
    }

    #[test]
    fn a_carried_default_takes_precedence_over_the_start_reminder() {
        let (mut items, _, tracker) = ticket_with_pull_request("approved");
        items.link(TICKET, "w1").expect("ticket");
        *tracker.start_reminder.lock().unwrap() = Some("Not assigned to you".into());

        let ticket = items.item_info(TICKET).expect("ticket");
        let ids: Vec<&str> = ticket
            .choices
            .iter()
            .map(|choice| choice.choice_id.as_str())
            .collect();
        assert_eq!(
            ids[..5],
            [
                "start_work",
                "mute_start_reminder",
                "pull_request:do",
                "pull_request:brief",
                "pull_request_open"
            ]
        );
        assert_eq!(ticket.default_choice_id.as_deref(), Some("pull_request:do"));
    }

    #[test]
    fn opening_the_pull_request_is_the_default_when_its_item_offers_no_default_to_carry() {
        let (items, pulls, _) = ticket_with_pull_request("approved");
        // E.g. every merge method is blocked: the item's default is opening it on GitHub.
        *pulls.default_choice.lock().unwrap() = Some("web".into());

        assert_eq!(
            items
                .item_info(TICKET)
                .expect("ticket")
                .default_choice_id
                .as_deref(),
            Some("pull_request_open")
        );
    }

    #[test]
    fn a_merged_or_closed_pull_request_carries_nothing_and_leaves_the_default_alone() {
        for status in ["merged", "closed"] {
            let (items, _, _) = ticket_with_pull_request(status);

            assert_eq!(
                choice_ids(&items, TICKET),
                ["pull_request_open", "local", "web", "do", "brief"],
                "{status}"
            );
            assert_eq!(
                items
                    .item_info(TICKET)
                    .expect("ticket")
                    .default_choice_id
                    .as_deref(),
                Some("web"),
                "{status}"
            );
        }
    }

    #[test]
    fn a_dismissed_pull_request_item_offers_nothing_on_its_ticket() {
        let (mut items, _, _) = ticket_with_pull_request("approved");
        items.hide("fake:pr:5", None).expect("item");

        assert_eq!(
            choice_ids(&items, TICKET),
            ["pull_request_open", "local", "web", "do", "brief"]
        );
        assert!(items.folded_choice(TICKET, "do").is_none());
    }

    #[test]
    fn a_carried_choice_that_finishes_does_not_leave_its_ticket_waiting_to_be_dropped() {
        let phase_after = |choice_id: &str| {
            let (mut items, _, _) = ticket_with_pull_request("approved");
            items.begin_action(TICKET, choice_id).expect("idle");
            items
                .finish_action(TICKET, Ok("Done".into()), Instant::now())
                .expect("running");
            items.get(TICKET).expect("ticket").phase
        };

        // The pull request item leaves once its source agrees, the ticket stays.
        assert_eq!(phase_after("pull_request:do"), WorkItemPhase::Pending);
        // The ticket's own choices still wait for the source to drop it.
        assert_eq!(phase_after("do"), WorkItemPhase::AwaitingExternal);
    }

    #[test]
    fn a_carried_choice_that_finishes_polls_the_pull_request_items_source_at_once() {
        let due_after = |choice_id: &str| {
            let (mut items, _, _) = ticket_with_pull_request("approved");
            let now = Instant::now();
            items.begin_action(TICKET, choice_id).expect("idle");
            items
                .finish_action(TICKET, Ok("Done".into()), now)
                .expect("running");
            let mut due: Vec<String> = items
                .take_due_polls(now)
                .iter()
                .map(|source| source.id().to_string())
                .collect();
            due.sort();
            due
        };

        assert_eq!(due_after("pull_request:do"), ["fake", "tracker"]);
        assert_eq!(due_after("do"), ["tracker"]);
    }

    /// The Jira ticket `TECH-7`, on a branch of its own, with the GitHub pull request `o/r#5`
    /// linked to it as `approved`. With `pull_request_item`, that pull request is also in the
    /// inbox on its own, ready to merge, and so folded into the ticket.
    fn jira_ticket_with_github_pull_request(pull_request_item: bool, workspace: bool) -> WorkItems {
        use crate::config::{
            GithubRepoConfig, GithubWorkItemsConfig, JiraProjectConfig, JiraWorkItemsConfig,
        };

        let github = Arc::new(github::GithubSource::new(
            GithubWorkItemsConfig {
                repos: vec![GithubRepoConfig {
                    name: "o/r".into(),
                    path: "/src/r".into(),
                    remote: "origin".into(),
                }],
                ..GithubWorkItemsConfig::default()
            },
            crate::config::AgentLaunch::default(),
        ));
        let jira = Arc::new(jira::JiraSource::new(
            JiraWorkItemsConfig {
                site: "example.atlassian.net".into(),
                email: "me@example.test".into(),
                projects: vec![JiraProjectConfig {
                    key: "TECH".into(),
                    path: "/src/app".into(),
                    remote: "origin".into(),
                    base_branch: None,
                    branch_template: "ar/{key}-{slug}".into(),
                }],
                ..JiraWorkItemsConfig::default()
            },
            crate::config::AgentLaunch::default(),
        ));
        let mut items = WorkItems::for_test(vec![github, jira], Instant::now());
        let now = Instant::now();
        let listed = |id: &str, title: &str| SourceItem {
            external_id: id.into(),
            title: title.into(),
            context: id.into(),
            author: None,
            url: format!("https://example.test/{id}"),
            updated_at: "2026-01-01T00:00:00Z".into(),
            tracker_state: None,
        };
        let prepare = |items: &mut WorkItems, key: &str, detail: serde_json::Value| {
            items.apply_event(
                WorkItemsEvent::Prepared {
                    key: key.into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    prepared: PreparedItem {
                        waiting: false,
                        detail: Some(detail),
                        summary: None,
                        error: None,
                        done: false,
                    },
                },
                now,
            );
        };
        poll_source(&mut items, "jira", vec![listed("TECH-7", "Add the thing")]);
        prepare(
            &mut items,
            "jira:TECH-7",
            serde_json::json!({
                "status": "In Progress",
                "status_category": "indeterminate",
                "assigned_to_me": true,
                "base_branch": "master",
                "existing_branch": "ar/TECH-7-thing",
            }),
        );
        if pull_request_item {
            poll_source(
                &mut items,
                "github",
                vec![listed("merge:o/r#5", "TECH-7 Add the thing")],
            );
            prepare(
                &mut items,
                "github:merge:o/r#5",
                serde_json::json!({
                    "number": 5,
                    "baseRefName": "main",
                    "headRefOid": "abc123",
                    "mergeStateStatus": "CLEAN",
                    "latestReviews": [
                        {"author": {"login": "tony"}, "state": "APPROVED", "body": ""}
                    ],
                    "mergeMethods": ["SQUASH", "MERGE", "REBASE"],
                }),
            );
        }
        if workspace {
            items.link("jira:TECH-7", "w1").expect("ticket");
        }
        items.apply_event(
            WorkItemsEvent::PullRequestsFound {
                results: vec![(
                    "jira:TECH-7".into(),
                    Ok(Some(crate::api::schema::WorkItemPullRequestInfo {
                        source_id: "github".into(),
                        repo: "o/r".into(),
                        number: 5,
                        url: "https://github.com/o/r/pull/5".into(),
                        is_draft: false,
                        status: "approved".into(),
                        stack: None,
                    })),
                )],
                own: Vec::new(),
            },
            now,
        );
        items
    }

    fn labels(items: &WorkItems, key: &str) -> Vec<String> {
        items
            .item_info(key)
            .expect("item")
            .choices
            .into_iter()
            .map(|choice| choice.label)
            .collect()
    }

    #[test]
    fn a_jira_ticket_with_a_ready_to_merge_pull_request_offers_its_merges_not_the_start_of_the_work(
    ) {
        let items = jira_ticket_with_github_pull_request(true, true);

        assert_eq!(
            labels(&items, "jira:TECH-7"),
            [
                "Squash and merge",
                "Ask agent to push and reply",
                "Create a merge commit",
                "Rebase and merge",
                "Open pull request",
                "Open in Jira"
            ]
        );
        let ticket = items.item_info("jira:TECH-7").expect("ticket");
        assert_eq!(
            ticket.default_choice_id.as_deref(),
            Some("pull_request:merge_squash")
        );
        // The merge asks twice, and the brief is free to go to the ticket's own agent.
        assert!(ticket.choices[0].confirm.is_some());
        assert_eq!(ticket.choices[1].disabled_reason, None);
    }

    #[test]
    fn without_a_workspace_the_ticket_keeps_continue_and_its_carried_brief_waits_for_one() {
        let items = jira_ticket_with_github_pull_request(true, false);

        assert_eq!(
            labels(&items, "jira:TECH-7"),
            [
                "Squash and merge",
                "Ask agent to push and reply",
                "Create a merge commit",
                "Rebase and merge",
                "Open pull request",
                "Continue on ar/TECH-7-thing",
                "Open in Jira"
            ]
        );
        let ticket = items.item_info("jira:TECH-7").expect("ticket");
        assert_eq!(
            ticket.choices[1].disabled_reason.as_deref(),
            Some("Work on it locally first")
        );
    }

    #[test]
    fn an_open_pull_request_without_its_own_item_or_a_workspace_is_opened_by_default() {
        let items = jira_ticket_with_github_pull_request(false, false);

        assert_eq!(
            labels(&items, "jira:TECH-7"),
            [
                "Open pull request",
                "Continue on ar/TECH-7-thing",
                "Open in Jira"
            ]
        );
        assert_eq!(
            items
                .item_info("jira:TECH-7")
                .expect("ticket")
                .default_choice_id
                .as_deref(),
            Some("pull_request_open")
        );
    }

    fn github_with_clone_root() -> WorkItemsConfig {
        WorkItemsConfig {
            github: Some(crate::config::GithubWorkItemsConfig {
                clone_root: "/projects".into(),
                ..crate::config::GithubWorkItemsConfig::default()
            }),
            ..WorkItemsConfig::default()
        }
    }

    fn review_request(items: &mut WorkItems) -> String {
        items.apply_event(
            WorkItemsEvent::Polled {
                source_id: "github".into(),
                result: Ok(vec![SourceItem {
                    external_id: "o/r#5".into(),
                    title: "Add the thing".into(),
                    context: "o/r #5".into(),
                    author: None,
                    url: "https://github.com/o/r/pull/5".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    tracker_state: None,
                }]),
            },
            Instant::now(),
        );
        items.state.items()[0].key.clone()
    }

    /// The switches the item's review choice offers.
    fn review_switches(items: &WorkItems, key: &str) -> Vec<String> {
        items
            .item_info(key)
            .expect("item")
            .choices
            .into_iter()
            .find(|choice| choice.choice_id == "review")
            .expect("review offered")
            .options
            .into_iter()
            .map(|option| option.option_id)
            .collect()
    }

    fn store_policy(name: &str, load: bool) -> StorePolicy {
        StorePolicy {
            path: std::env::temp_dir()
                .join(format!("herdr-linked-clones-{}-{name}", std::process::id()))
                .join("work-items.json"),
            load,
            persist: false,
        }
    }

    #[test]
    fn a_linked_clone_maps_its_repository_and_leaves_the_item_to_work_on() {
        let now = Instant::now();
        let mut items = WorkItems::from_config(
            &github_with_clone_root(),
            store_policy("link", false),
            &HashSet::new(),
            now,
        );
        let key = review_request(&mut items);
        items
            .begin_action(&key, source::LINK_CLONE_CHOICE_ID)
            .expect("starts");
        items.apply_event(
            WorkItemsEvent::CloneLinked {
                key: key.clone(),
                result: Ok((
                    LinkedClone {
                        source_id: "github".into(),
                        name: "o/r".into(),
                        path: "/projects/r".into(),
                        remote: "origin".into(),
                    },
                    "Linked o/r to /projects/r".into(),
                )),
            },
            now,
        );

        // Linked, the repository is mapped, so a review can have its worktree.
        assert_eq!(review_switches(&items, &key), ["worktree", "post"]);
        assert_eq!(
            items.state.get(&key).expect("item").phase,
            WorkItemPhase::Pending
        );
        assert_eq!(
            items
                .repositories
                .iter()
                .map(|repository| repository.info.path.as_str())
                .collect::<Vec<_>>(),
            ["/projects/r"]
        );
    }

    #[test]
    fn clones_linked_before_a_restart_stay_linked() {
        let policy = store_policy("restart", true);
        let parent = policy.path.parent().expect("parent").to_path_buf();
        std::fs::create_dir_all(&parent).expect("creates");
        std::fs::write(
            &policy.path,
            r#"{"version":1,"items":[],"linked_clones":[
                {"source_id":"github","name":"o/r","path":"/projects/r","remote":"origin"}]}"#,
        )
        .expect("writes");
        let mut items = WorkItems::from_config(
            &github_with_clone_root(),
            policy,
            &HashSet::new(),
            Instant::now(),
        );
        let _ = std::fs::remove_dir_all(parent);
        let key = review_request(&mut items);

        assert_eq!(review_switches(&items, &key), ["worktree", "post"]);
    }

    /// The inbox with ticket `fake:a` started in workspace `w1`, whose provisioning finished
    /// with its agent brief failed at the moment returned.
    fn item_with_failed_brief() -> (WorkItems, Instant) {
        use crate::api::schema::{WorkItemStep, WorkItemStepInfo, WorkItemStepStatus};

        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        poll(&mut items, &["a"]);
        // Ahead of now, so moments before it are safe to make.
        let failed_at = Instant::now() + Duration::from_secs(3_600);
        let item = items.state.get_mut("fake:a").expect("item");
        item.phase = WorkItemPhase::Local;
        item.workspace_id = Some("w1".into());
        item.provisioning = Some(WorkItemProvisioningInfo {
            steps: vec![
                WorkItemStepInfo {
                    step: WorkItemStep::Checkout,
                    label: "Worktree created".into(),
                    status: WorkItemStepStatus::Done,
                    detail: None,
                },
                WorkItemStepInfo {
                    step: WorkItemStep::AgentBrief,
                    label: "Agent briefed".into(),
                    status: WorkItemStepStatus::Failed,
                    detail: Some("agent still idle 15 s after the brief".into()),
                },
            ],
            finished: true,
            finished_at: Some(1_000),
        });
        item.brief_failed_at = Some(failed_at);
        (items, failed_at)
    }

    /// What the agents of workspace `w1` mean for you when the latest turn of one ended at
    /// `turn_ended`.
    fn agents_in_w1(turn_ended: Option<Instant>) -> HashMap<String, attention::AgentVerdict> {
        HashMap::from([(
            "w1".to_string(),
            attention::AgentVerdict {
                turn_finished_at: turn_ended,
                ..attention::AgentVerdict::default()
            },
        )])
    }

    fn attention_of_a(items: &WorkItems) -> Option<AttentionKind> {
        items
            .attention(&attention::Subject::Item("fake:a".into()))
            .map(|attention| attention.kind)
    }

    #[test]
    fn a_failed_agent_brief_stops_needing_you_once_an_agent_took_a_turn_after_it() {
        let (mut items, failed_at) = item_with_failed_brief();
        let mut attention_after = |turn_ended: Option<Instant>| {
            items.update_attention(&agents_in_w1(turn_ended), Vec::new(), 2_000);
            attention_of_a(&items)
        };
        assert_eq!(attention_after(None), Some(AttentionKind::Failed));
        // The agent worked after the failure, so it was ready after all.
        assert_eq!(
            attention_after(Some(failed_at + Duration::from_secs(5))),
            None
        );
        // For good: the failure does not return once that agent is gone.
        assert_eq!(attention_after(None), None);

        // Only the attention clears; the step stays failed.
        let steps = &items
            .get("fake:a")
            .expect("item")
            .provisioning
            .as_ref()
            .expect("provisioning")
            .steps;
        assert_eq!(
            steps[1].status,
            crate::api::schema::WorkItemStepStatus::Failed
        );
    }

    #[test]
    fn a_turn_that_ended_before_the_failure_never_clears_it_whatever_the_time() {
        let (mut items, failed_at) = item_with_failed_brief();
        // E.g. answering the agent's folder trust prompt, before the brief failed. However far
        // the wall clock moves meanwhile, as over a night the machine sleeps, it stays before.
        let before = agents_in_w1(Some(failed_at - Duration::from_secs(5)));
        for now_unix in [0, 2_000, 4_000_000_000, u64::MAX / 2] {
            items.update_attention(&before, Vec::new(), now_unix);
            assert_eq!(
                attention_of_a(&items),
                Some(AttentionKind::Failed),
                "at Unix time {now_unix}"
            );
        }
    }

    #[test]
    fn a_turn_does_not_make_up_for_a_workspace_that_failed_to_come_up() {
        use crate::api::schema::WorkItemStepStatus;

        let (mut items, failed_at) = item_with_failed_brief();
        let provisioning = items
            .state
            .get_mut("fake:a")
            .and_then(|item| item.provisioning.as_mut())
            .expect("provisioning");
        provisioning.steps[0].status = WorkItemStepStatus::Failed;
        provisioning.steps[1].status = WorkItemStepStatus::Skipped;

        items.update_attention(
            &agents_in_w1(Some(failed_at + Duration::from_secs(5))),
            Vec::new(),
            2_000,
        );
        assert_eq!(attention_of_a(&items), Some(AttentionKind::Failed));
    }

    #[test]
    fn a_new_attempt_whose_brief_fails_needs_you_until_a_turn_after_that_failure() {
        use crate::api::schema::{WorkItemStep, WorkItemStepStatus};

        let (mut items, failed_at) = item_with_failed_brief();
        items.update_attention(
            &agents_in_w1(Some(failed_at + Duration::from_secs(5))),
            Vec::new(),
            2_000,
        );
        assert_eq!(attention_of_a(&items), None);

        // The user starts over, and this time the brief fails after the agent's last turn.
        let last_turn = Instant::now();
        let plan = ProvisionPlan {
            source: source::WorkspaceSource::Scratch(PathBuf::from("/scratch")),
            workspace_label: "#1 Title".into(),
            agent_name_hint: "agent-1".into(),
            brief: "brief".into(),
            plan_command: String::new(),
            layout: source::WorkspaceLayout {
                agent: "claude".into(),
                agent_args: Vec::new(),
                tabs: Vec::new(),
                diff_command: String::new(),
            },
            delete_branch: false,
            shared_with: Vec::new(),
        };
        let job_id = items.start_job("fake:a", plan).expect("job");
        items.update_progress(job_id, |progress| {
            provision::set_step(
                progress,
                WorkItemStep::Checkout,
                WorkItemStepStatus::Done,
                None,
            );
            provision::set_step(
                progress,
                WorkItemStep::AgentBrief,
                WorkItemStepStatus::Failed,
                Some("agent still idle 15 s after the brief".into()),
            );
        });
        items.update_attention(&agents_in_w1(Some(last_turn)), Vec::new(), 2_000);
        assert_eq!(attention_of_a(&items), Some(AttentionKind::Failed));

        // A turn after this failure settles it in turn.
        items.update_attention(
            &agents_in_w1(Some(Instant::now() + Duration::from_secs(1))),
            Vec::new(),
            2_000,
        );
        assert_eq!(attention_of_a(&items), None);
    }

    #[test]
    fn a_workspace_shared_by_several_items_is_removed_only_once_the_last_one_resolves() {
        let source = FakeSource::with_items(Vec::new());
        source
            .remove_on_resolved
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut items = WorkItems::for_test(vec![source], Instant::now());
        poll(&mut items, &["a", "b", "c"]);
        let plan = ProvisionPlan {
            source: source::WorkspaceSource::Scratch(PathBuf::from("/scratch")),
            workspace_label: "#a–#b".into(),
            agent_name_hint: "review-stack".into(),
            brief: "brief".into(),
            plan_command: String::new(),
            layout: source::WorkspaceLayout {
                agent: "claude".into(),
                agent_args: Vec::new(),
                tabs: Vec::new(),
                diff_command: String::new(),
            },
            delete_branch: false,
            shared_with: vec!["fake:b".into()],
        };
        let job_id = items.start_job("fake:a", plan).expect("job");
        assert!(
            items.has_job("fake:b"),
            "b cannot get a workspace of its own meanwhile"
        );
        items.link_workspace(job_id, "w1");
        let workspace = |items: &WorkItems, key: &str| {
            items.get(key).and_then(|item| item.workspace_id.clone())
        };
        assert_eq!(workspace(&items, "fake:a").as_deref(), Some("w1"));
        assert_eq!(workspace(&items, "fake:b").as_deref(), Some("w1"));
        assert_eq!(workspace(&items, "fake:c"), None);

        // b's review is done first: a still works in the workspace.
        poll(&mut items, &["a", "c"]);
        assert!(items.get("fake:b").is_some_and(|item| item.resolved));
        assert_eq!(items.take_pending_resolutions(), Vec::<String>::new());

        poll(&mut items, &["c"]);
        assert_eq!(items.take_pending_resolutions(), ["fake:a"]);

        // Removing it takes both resolved items along.
        items.workspace_closed("w1");
        assert!(items.get("fake:a").is_none());
        assert!(items.get("fake:b").is_none());
    }

    #[test]
    fn closing_a_shared_workspace_returns_every_unresolved_item_to_the_inbox() {
        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        poll(&mut items, &["a", "b"]);
        for key in ["fake:a", "fake:b"] {
            let item = items.state.get_mut(key).expect("item");
            item.workspace_id = Some("w1".into());
            item.phase = WorkItemPhase::Local;
        }
        items.workspace_closed("w1");
        for key in ["fake:a", "fake:b"] {
            let item = items.get(key).expect("kept");
            assert_eq!(
                (item.workspace_id.as_deref(), item.phase),
                (None, WorkItemPhase::Pending)
            );
        }
    }

    #[test]
    fn an_agent_in_a_shared_workspace_needs_you_through_one_item() {
        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        poll(&mut items, &["a", "b"]);
        for key in ["fake:a", "fake:b"] {
            items.state.get_mut(key).expect("item").workspace_id = Some("w1".into());
        }
        items.state.get_mut("fake:a").expect("item").resolved = true;
        let blocked = HashMap::from([(
            "w1".to_string(),
            attention::AgentVerdict {
                need: Some(attention::Need::new(AttentionKind::Blocked, "allow?")),
                ..attention::AgentVerdict::default()
            },
        )]);
        items.update_attention(&blocked, Vec::new(), 1_000);
        let kind = |key: &str| {
            items
                .attention(&attention::Subject::Item(key.into()))
                .map(|attention| attention.kind)
        };
        // Through the one still under review, not once per item.
        assert_eq!(kind("fake:b"), Some(AttentionKind::Blocked));
        assert_eq!(kind("fake:a"), None);
    }

    fn fake_project(project: &str) -> WorkItemProject {
        WorkItemProject {
            source_id: "fake".into(),
            project: project.into(),
        }
    }

    fn keys(items: &WorkItems) -> Vec<&str> {
        let mut keys: Vec<&str> = items
            .state
            .items()
            .iter()
            .map(|item| item.key.as_str())
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn an_ignored_project_leaves_the_inbox_and_stays_out_of_later_polls() {
        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        poll(&mut items, &["work/1", "personal/2"]);

        items
            .ignore_project(&fake_project("PERSONAL"))
            .expect("ignored");
        assert_eq!(keys(&items), ["fake:work/1"]);

        let notices = poll(&mut items, &["work/1", "personal/2", "personal/3"]);
        assert_eq!(keys(&items), ["fake:work/1"]);
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[test]
    fn work_started_on_an_ignored_project_is_followed_to_its_end() {
        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        poll(&mut items, &["personal/2"]);
        items
            .state
            .get_mut("fake:personal/2")
            .expect("item")
            .workspace_id = Some("w1".into());

        items
            .ignore_project(&fake_project("personal"))
            .expect("ignored");
        poll(&mut items, &["personal/2"]);
        assert_eq!(keys(&items), ["fake:personal/2"]);
        assert!(!items.get("fake:personal/2").expect("item").resolved);

        // Its source stops listing it: it resolves like any other.
        poll(&mut items, &[]);
        assert!(items.get("fake:personal/2").expect("item").resolved);
    }

    #[test]
    fn an_unignored_project_comes_back_at_the_next_poll_which_is_due_at_once() {
        let start = Instant::now();
        let mut items = WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], start);
        assert_eq!(items.take_due_polls(start).len(), 1);
        poll(&mut items, &["personal/2"]);
        items
            .ignore_project(&fake_project("personal"))
            .expect("ignored");
        assert!(items.take_due_polls(Instant::now()).is_empty());

        items.unignore_project(&fake_project("Personal"));
        assert!(items.ignored_projects().is_empty());
        assert_eq!(items.take_due_polls(Instant::now()).len(), 1);
        poll(&mut items, &["personal/2"]);
        assert_eq!(keys(&items), ["fake:personal/2"]);
    }

    #[test]
    fn only_a_configured_source_and_a_named_project_can_be_ignored() {
        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        let unknown = WorkItemProject {
            source_id: "jira".into(),
            project: "APP".into(),
        };
        assert_eq!(
            items.ignore_project(&unknown).map_err(|(code, _)| code),
            Err("work_item_source_not_found")
        );
        assert_eq!(
            items
                .ignore_project(&fake_project("  "))
                .map_err(|(code, _)| code),
            Err("invalid_params")
        );
        assert!(items.ignored_projects().is_empty());
    }

    #[test]
    fn items_report_the_project_they_belong_to() {
        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        poll(&mut items, &["work/1", "loose"]);
        let project = |key: &str| items.item_info(key).expect("item").project;
        assert_eq!(project("fake:work/1").as_deref(), Some("work"));
        assert_eq!(project("fake:loose"), None);
    }

    #[test]
    fn a_local_item_is_dated_in_the_form_the_sources_date_theirs() {
        assert_eq!(rfc3339_utc(1_767_225_600), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn local_items_and_the_numbers_given_out_survive_a_restart() {
        let dir =
            std::env::temp_dir().join(format!("herdr-local-items-{}-restart", std::process::id()));
        let policy = StorePolicy {
            path: dir.join("work-items.json"),
            load: true,
            persist: true,
        };
        let config = github_with_clone_root();
        let mut items =
            WorkItems::from_config(&config, policy.clone(), &HashSet::new(), Instant::now());
        let kept = items.create_local("Fix login", None);
        let finished = items.create_local("Tidy up", Some("w1"));
        // Done, and its workspace closes: gone, but its number is not free again.
        items
            .set_local_resolved(&finished, true)
            .expect("local item");
        items.workspace_closed("w1");
        assert_eq!((kept.as_str(), finished.as_str()), ("local:1", "local:2"));

        // The store is written on a thread of its own: wait for the last state to land.
        let written = |policy: &StorePolicy| {
            let stored = store::load(&policy.path);
            stored.last_local_number == 2 && stored.items.len() == 1
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !written(&policy) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(written(&policy), "the inbox was not stored in time");
        drop(items);

        let policy = StorePolicy {
            persist: false,
            ..policy
        };
        let mut restarted =
            WorkItems::from_config(&config, policy, &HashSet::new(), Instant::now());
        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(
            restarted.item_info(&kept).map(|item| item.title),
            Some("Fix login".to_string())
        );
        assert!(restarted.item_info(&finished).is_none());
        assert_eq!(restarted.create_local("Next", None), "local:3");
    }

    #[test]
    fn a_local_item_titled_with_a_ticket_key_is_not_linked_to_that_ticket() {
        let mut items = with_tracker();
        let local = items.create_local("T-1 Fix login", None);
        // An item of another source with the same title is looked up, so the setup is sound.
        poll_source(&mut items, "fake", vec![titled("pr", "T-1 Fix login")]);

        let lookups = items
            .take_due_ticket_lookup(Instant::now())
            .expect("lookup due");
        let asked: Vec<&str> = lookups.iter().map(|(item, _, _)| item.as_str()).collect();
        assert_eq!(asked, ["fake:pr"]);
        assert_eq!(linked_ticket(&items, &local), None);
    }

    #[test]
    fn a_done_local_item_needs_nothing_whatever_its_agents_do() {
        let mut items =
            WorkItems::for_test(vec![FakeSource::with_items(Vec::new())], Instant::now());
        let key = items.create_local("Fix login", Some("w1"));
        let finished = HashMap::from([(
            "w1".to_string(),
            attention::AgentVerdict {
                need: Some(attention::Need::new(
                    AttentionKind::Finished,
                    "claude finished its turn",
                )),
                ..attention::AgentVerdict::default()
            },
        )]);
        let attention_of = |items: &WorkItems| {
            items
                .attention(&attention::Subject::Item(key.clone()))
                .map(|attention| attention.kind)
        };

        items.update_attention(&finished, Vec::new(), 2_000);
        assert_eq!(attention_of(&items), Some(AttentionKind::Finished));

        items.set_local_resolved(&key, true).expect("local item");
        items.update_attention(&finished, Vec::new(), 2_001);
        assert_eq!(attention_of(&items), None);

        // Not done after all: what the agent finished needs you again.
        items.set_local_resolved(&key, false).expect("local item");
        items.update_attention(&finished, Vec::new(), 2_002);
        assert_eq!(attention_of(&items), Some(AttentionKind::Finished));
    }
}
