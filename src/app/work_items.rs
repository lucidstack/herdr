//! App-thread driver for work items: schedules polls, preparation and local
//! provisioning on background threads and applies their results.
//!
//! Provisioning drives the workspace through the public API methods so it stays
//! decoupled from internal creation helpers.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::{App, AppPolicy};
use crate::api::schema::{
    AgentPromptParams, AgentStartParams, ErrorBody, ErrorResponse, Method, PaneInfo,
    PaneSendInputParams, Request, ResponseResult, SuccessResponse, TabCreateParams, TabInfo,
    TabRenameParams, WorkItemStep, WorkItemStepStatus, WorkspaceCloseParams, WorkspaceCreateParams,
    WorkspaceInfo, WorktreeCreateParams, WorktreeOpenParams, WorktreeRemoveParams,
};
use crate::events::AppEvent;
use crate::work_items::provision::{self, AgentAttempt, PendingResponse, SourceReady};
use crate::work_items::source::WorkspaceSource;
use crate::work_items::{
    OwnedWorktree, PendingRemoval, StorePolicy, WorkItemNotice, WorkItemsEvent,
};

/// How long the agent may take to accept `agent.start` and then its brief.
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(90);
const AGENT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const BRIEF_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How often a deferred worktree request is checked for its response.
const RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_AGENT_NAME_SUFFIX: usize = 9;

pub(super) fn store_policy(policy: AppPolicy) -> StorePolicy {
    StorePolicy {
        path: crate::session::data_dir().join("work-items.json"),
        load: policy.restore_session,
        persist: policy.persist_session,
    }
}

fn send_event(event_tx: &tokio::sync::mpsc::Sender<AppEvent>, event: WorkItemsEvent) {
    let _ = event_tx.blocking_send(AppEvent::WorkItems(Box::new(event)));
}

fn parse_response(response: &str) -> Result<ResponseResult, ErrorBody> {
    if let Ok(success) = serde_json::from_str::<SuccessResponse>(response) {
        return Ok(success.result);
    }
    match serde_json::from_str::<ErrorResponse>(response) {
        Ok(error) => Err(error.error),
        Err(err) => Err(ErrorBody {
            code: "invalid_response".into(),
            message: err.to_string(),
        }),
    }
}

impl App {
    pub(super) fn apply_work_items_config(&mut self, config: &crate::config::WorkItemsConfig) {
        let existing: HashSet<&str> = self
            .state
            .workspaces
            .iter()
            .map(|ws| ws.id.as_str())
            .collect();
        self.work_items
            .apply_config(config, store_policy(self.policy), &existing, Instant::now());
    }

    pub(super) fn work_items_workspace_closed(&mut self, workspace_id: &str) {
        self.work_items.workspace_closed(workspace_id);
    }

    /// Deletes the review branch of a removed worktree when its workflow asks for it.
    pub(super) fn work_items_worktree_removed(&mut self, checkout_path: &str) {
        let Some(owned) = self.work_items.take_owned_worktree(checkout_path) else {
            return;
        };
        if !owned.delete_branch {
            return;
        }
        std::thread::spawn(move || {
            if let Err(err) = provision::delete_branch(&owned.repo_path, &owned.branch) {
                tracing::warn!(
                    branch = %owned.branch,
                    repo = %owned.repo_path.display(),
                    err = %err,
                    "failed to delete review branch"
                );
            }
        });
    }

    fn request_work_items_render(&self) {
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Starts due polls and advances agent start and brief attempts. Returns whether
    /// visible state changed.
    pub(crate) fn run_work_items_tasks(&mut self, now: Instant) -> bool {
        if !self.work_items.is_enabled() {
            return false;
        }
        for source in self.work_items.take_due_polls(now) {
            let event_tx = self.event_tx.clone();
            std::thread::spawn(move || {
                let result = source.poll();
                send_event(
                    &event_tx,
                    WorkItemsEvent::Polled {
                        source_id: source.id().to_string(),
                        result,
                    },
                );
            });
        }
        let revision = self.work_items.revision();
        for job_id in self.work_items.job_ids() {
            self.advance_work_item_worktree(job_id, now);
            self.advance_work_item_agent(job_id, now);
        }
        self.advance_work_item_removals(now);
        let changed = self.work_items.revision() != revision;
        if changed {
            self.request_work_items_render();
        }
        changed
    }

    pub(crate) fn handle_work_items_event(
        &mut self,
        event: WorkItemsEvent,
    ) -> (bool, Vec<WorkItemNotice>) {
        let revision = self.work_items.revision();
        let now = Instant::now();
        let mut notices = Vec::new();
        match event {
            WorkItemsEvent::CheckoutFinished { job_id, result } => {
                self.work_item_source_ready(job_id, result, now);
            }
            event => {
                let polled_source = match &event {
                    WorkItemsEvent::Polled {
                        source_id,
                        result: Ok(_),
                    } => Some(source_id.clone()),
                    _ => None,
                };
                notices = self.work_items.apply_event(event, now).1;
                if let Some(source_id) = polled_source {
                    self.start_work_items_prepare(&source_id);
                }
                self.start_work_item_resolutions(now);
            }
        }
        notices.extend(self.work_items.take_notices());
        let changed = self.work_items.revision() != revision;
        if changed {
            self.request_work_items_render();
        }
        (changed, notices)
    }

    fn start_work_items_prepare(&mut self, source_id: &str) {
        let Some(source) = self.work_items.source(source_id).cloned() else {
            return;
        };
        let pending = self.work_items.take_needs_prepare(source_id);
        if pending.is_empty() {
            return;
        }
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            for item in pending {
                let prepared = source.prepare(&item);
                send_event(
                    &event_tx,
                    WorkItemsEvent::Prepared {
                        key: crate::work_items::state::item_key(source.id(), &item.external_id),
                        updated_at: item.updated_at,
                        prepared,
                    },
                );
            }
        });
    }

    /// Validates and starts local provisioning for `key`. Errors are `(code, message)`.
    pub(super) fn start_work_item_provisioning(
        &mut self,
        key: &str,
        choice_id: &str,
    ) -> Result<(), (&'static str, String)> {
        if self.work_items.has_job(key) {
            return Err(("work_item_busy", format!("{key} is already being prepared")));
        }
        let Some(item) = self.work_items.get(key).cloned() else {
            return Err(("work_item_not_found", format!("unknown work item {key}")));
        };
        if item
            .workspace_id
            .as_deref()
            .is_some_and(|workspace_id| self.parse_workspace_id(workspace_id).is_some())
        {
            return Err((
                "work_item_already_provisioned",
                format!("{key} already has a workspace"),
            ));
        }
        let Some(source) = self.work_items.source(&item.source_id).cloned() else {
            return Err(("work_item_not_found", format!("unknown work item {key}")));
        };
        let plan = source
            .provision_plan(&item, choice_id, &self.state.worktree_directory)
            .map_err(|message| ("work_item_unavailable", message))?;
        let workspace_source = plan.source.clone();
        let job_id = self
            .work_items
            .start_job(key, plan)
            .map_err(|_| ("work_item_not_found", format!("unknown work item {key}")))?;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = provision::prepare_source(&workspace_source);
            send_event(
                &event_tx,
                WorkItemsEvent::CheckoutFinished { job_id, result },
            );
        });
        self.request_work_items_render();
        Ok(())
    }

    fn work_items_api(&mut self, method: Method) -> Result<ResponseResult, ErrorBody> {
        let request = Request {
            id: "work-items".into(),
            method,
        };
        parse_response(&self.handle_api_request_after_internal_events_drained(request))
    }

    fn fail_work_item_workspace(&mut self, job_id: u64, detail: String) {
        self.work_items.update_progress(job_id, |progress| {
            provision::fail_checkout(progress, detail)
        });
        self.work_items.finish_job_if_done(job_id);
    }

    /// The background preparation finished: create or reopen the workspace.
    fn work_item_source_ready(
        &mut self,
        job_id: u64,
        result: Result<SourceReady, String>,
        now: Instant,
    ) {
        let Some(job) = self.work_items.job(job_id) else {
            return;
        };
        let plan = job.plan.clone();
        let ready = match result {
            Ok(ready) => ready,
            Err(err) => return self.fail_work_item_workspace(job_id, err),
        };
        match (&plan.source, ready) {
            (WorkspaceSource::Worktree(spec), SourceReady::NewBranch { branch, base }) => {
                // worktree.create runs through Herdr so worktree.created hooks fire.
                let (tx, rx) = std::sync::mpsc::channel();
                let request = Request {
                    id: "work-items".into(),
                    method: Method::WorktreeCreate(WorktreeCreateParams {
                        workspace_id: None,
                        cwd: Some(spec.repo_path.display().to_string()),
                        branch: Some(branch.clone()),
                        base: Some(base),
                        path: None,
                        label: Some(plan.workspace_label.clone()),
                        focus: false,
                        trust_repository: false,
                    }),
                };
                if let Some(job) = self.work_items.job_mut(job_id) {
                    job.branch = Some(branch);
                    job.worktree = Some(PendingResponse {
                        next_check: now + RESPONSE_POLL_INTERVAL,
                        response: rx,
                    });
                }
                if !self.handle_deferred_worktree_api_request(request, tx) {
                    self.fail_work_item_workspace(job_id, "worktree.create is unavailable".into());
                }
            }
            (WorkspaceSource::Worktree(spec), SourceReady::ExistingWorktree(path)) => {
                let opened = self.work_items_api(Method::WorktreeOpen(WorktreeOpenParams {
                    workspace_id: None,
                    cwd: Some(spec.repo_path.display().to_string()),
                    path: Some(path.display().to_string()),
                    branch: None,
                    label: Some(plan.workspace_label.clone()),
                    focus: false,
                    trust_repository: false,
                }));
                match opened {
                    Ok(ResponseResult::WorktreeOpened {
                        workspace,
                        tab,
                        root_pane,
                        worktree,
                        ..
                    }) => {
                        if let Some(job) = self.work_items.job_mut(job_id) {
                            job.branch = Some(spec.branch.clone());
                        }
                        self.work_item_workspace_ready(
                            job_id,
                            workspace,
                            tab,
                            root_pane,
                            &worktree.path,
                            Some(format!("reopened {}", spec.branch)),
                            now,
                        );
                    }
                    Ok(_) => self.fail_work_item_workspace(
                        job_id,
                        unexpected_response("worktree.open").message,
                    ),
                    Err(err) => self.fail_work_item_workspace(
                        job_id,
                        format!("worktree.open: {}", err.message),
                    ),
                }
            }
            (WorkspaceSource::Download(spec), SourceReady::Downloaded) => {
                let cwd = spec.directory.display().to_string();
                match self.work_items_api(Method::WorkspaceCreate(WorkspaceCreateParams {
                    source_workspace_id: None,
                    cwd: Some(cwd.clone()),
                    focus: false,
                    label: Some(plan.workspace_label.clone()),
                    env: Default::default(),
                })) {
                    Ok(ResponseResult::WorkspaceCreated {
                        workspace,
                        tab,
                        root_pane,
                    }) => self.work_item_workspace_ready(
                        job_id, workspace, tab, root_pane, &cwd, None, now,
                    ),
                    Ok(_) => self.fail_work_item_workspace(
                        job_id,
                        unexpected_response("workspace.create").message,
                    ),
                    Err(err) => self.fail_work_item_workspace(
                        job_id,
                        format!("workspace.create: {}", err.message),
                    ),
                }
            }
            (_, ready) => {
                self.fail_work_item_workspace(job_id, format!("unexpected preparation {ready:?}"))
            }
        }
    }

    /// Checks a pending `worktree.create` and continues once Herdr answers.
    fn advance_work_item_worktree(&mut self, job_id: u64, now: Instant) {
        let Some(pending) = self
            .work_items
            .job_mut(job_id)
            .and_then(|job| job.worktree.as_mut())
        else {
            return;
        };
        if pending.next_check > now {
            return;
        }
        let response = match pending.response.try_recv() {
            Ok(response) => response,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                pending.next_check = now + RESPONSE_POLL_INTERVAL;
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return self
                    .fail_work_item_workspace(job_id, "worktree.create response was lost".into());
            }
        };
        let Some(job) = self.work_items.job_mut(job_id) else {
            return;
        };
        job.worktree = None;
        let WorkspaceSource::Worktree(spec) = job.plan.source.clone() else {
            return;
        };
        let delete_branch = job.plan.delete_branch;
        let branch = job.branch.clone().unwrap_or_else(|| spec.branch.clone());
        match parse_response(&response) {
            Ok(ResponseResult::WorktreeCreated {
                workspace,
                tab,
                root_pane,
                worktree,
            }) => {
                self.work_items.record_owned_worktree(OwnedWorktree {
                    checkout_path: worktree.path.clone(),
                    repo_path: spec.repo_path.clone(),
                    branch: branch.clone(),
                    delete_branch,
                });
                self.work_item_workspace_ready(
                    job_id,
                    workspace,
                    tab,
                    root_pane,
                    &worktree.path,
                    Some(format!("on {branch}")),
                    now,
                );
            }
            Ok(_) => self
                .fail_work_item_workspace(job_id, unexpected_response("worktree.create").message),
            Err(err) => {
                self.fail_work_item_workspace(job_id, format!("worktree.create: {}", err.message))
            }
        }
    }

    /// The workspace exists: link it to the item, add the tool tabs and start the agent.
    #[allow(clippy::too_many_arguments)] // One call per provisioning path; a struct would only rename the fields.
    fn work_item_workspace_ready(
        &mut self,
        job_id: u64,
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
        cwd: &str,
        detail: Option<String>,
        now: Instant,
    ) {
        self.work_items
            .link_workspace(job_id, &workspace.workspace_id);
        self.work_items.update_progress(job_id, |progress| {
            provision::set_step(
                progress,
                WorkItemStep::Checkout,
                WorkItemStepStatus::Done,
                detail,
            )
        });
        if let Err(err) = self.add_work_item_tabs(job_id, &workspace.workspace_id, tab, cwd) {
            let detail = format!("workspace setup failed: {}", err.message);
            self.work_items.update_progress(job_id, |progress| {
                provision::end_unfinished(progress, WorkItemStepStatus::Failed, &detail)
            });
            self.work_items.finish_job_if_done(job_id);
            return;
        }
        if let Some(job) = self.work_items.job_mut(job_id) {
            job.agent_pane_id = Some(root_pane.pane_id);
        }
        self.start_work_item_agent(job_id, now);
        self.work_items.finish_job_if_done(job_id);
    }

    fn add_work_item_tabs(
        &mut self,
        job_id: u64,
        workspace_id: &str,
        first_tab: TabInfo,
        cwd: &str,
    ) -> Result<(), ErrorBody> {
        let Some(job) = self.work_items.job(job_id) else {
            return Ok(());
        };
        let plan = job.plan.clone();
        let layout = &plan.layout;
        self.work_items_api(Method::TabRename(TabRenameParams {
            tab_id: first_tab.tab_id,
            label: if layout.agent.is_empty() {
                "shell"
            } else {
                "agent"
            }
            .into(),
        }))?;
        let tools: Vec<(&str, String)> = match &plan.source {
            WorkspaceSource::Worktree(_) => vec![
                ("editor", layout.editor_command.clone()),
                ("lazygit", layout.lazygit_command.clone()),
            ],
            WorkspaceSource::Download(download) => vec![(
                "diff",
                layout.diff_command.replace("{file}", &download.file_name),
            )],
        };
        for (label, command) in tools {
            if command.is_empty() {
                continue;
            }
            let pane_id = self.create_work_item_tab(workspace_id, cwd, label)?;
            self.work_items_api(Method::PaneSendInput(PaneSendInputParams {
                pane_id,
                text: command,
                keys: vec!["enter".into()],
            }))?;
        }
        Ok(())
    }

    fn create_work_item_tab(
        &mut self,
        workspace_id: &str,
        cwd: &str,
        label: &str,
    ) -> Result<String, ErrorBody> {
        match self.work_items_api(Method::TabCreate(TabCreateParams {
            workspace_id: Some(workspace_id.to_string()),
            cwd: Some(cwd.to_string()),
            focus: false,
            label: Some(label.to_string()),
            env: Default::default(),
        }))? {
            ResponseResult::TabCreated { root_pane, .. } => Ok(root_pane.pane_id),
            _ => Err(unexpected_response("tab.create")),
        }
    }

    /// Removes the workspaces of items that resolved under an `on_resolved = "remove"` workflow.
    fn start_work_item_resolutions(&mut self, now: Instant) {
        for key in self.work_items.take_pending_resolutions() {
            let Some(workspace_id) = self
                .work_items
                .get(&key)
                .and_then(|item| item.workspace_id.clone())
            else {
                continue;
            };
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                continue;
            };
            let linked_worktree = self.state.workspaces[ws_idx]
                .worktree_space()
                .is_some_and(|space| space.is_linked_worktree);
            if !linked_worktree {
                if let Err(err) =
                    self.work_items_api(Method::WorkspaceClose(WorkspaceCloseParams {
                        workspace_id,
                        close_group: false,
                    }))
                {
                    self.work_items
                        .set_resolve_error(&key, Some(format!("not closed: {}", err.message)));
                }
                continue;
            }
            // Refuses to discard uncommitted changes; worktree.removed hooks and the
            // branch policy run once Herdr removes it.
            let (tx, rx) = std::sync::mpsc::channel();
            let request = Request {
                id: "work-items".into(),
                method: Method::WorktreeRemove(WorktreeRemoveParams {
                    workspace_id,
                    force: false,
                    trust_repository: false,
                }),
            };
            if self.handle_deferred_worktree_api_request(request, tx) {
                self.work_items.start_removal(PendingRemoval {
                    key,
                    pending: PendingResponse {
                        next_check: now + RESPONSE_POLL_INTERVAL,
                        response: rx,
                    },
                });
            }
        }
    }

    fn advance_work_item_removals(&mut self, now: Instant) {
        let mut failures = Vec::new();
        self.work_items.removals_mut().retain_mut(|removal| {
            if removal.pending.next_check > now {
                return true;
            }
            match removal.pending.response.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    removal.pending.next_check = now + RESPONSE_POLL_INTERVAL;
                    true
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
                Ok(response) => {
                    if let Err(err) = parse_response(&response) {
                        failures.push((removal.key.clone(), err.message));
                    }
                    false
                }
            }
        });
        for (key, message) in failures {
            self.work_items
                .set_resolve_error(&key, Some(format!("worktree kept: {message}")));
        }
    }

    fn set_work_item_agent_step(
        &mut self,
        job_id: u64,
        status: WorkItemStepStatus,
        detail: Option<String>,
    ) {
        self.work_items.update_progress(job_id, |progress| {
            provision::set_step(progress, WorkItemStep::AgentBrief, status, detail)
        });
    }

    fn start_work_item_agent(&mut self, job_id: u64, now: Instant) {
        let pending = self
            .work_items
            .progress(job_id)
            .and_then(|progress| provision::status(progress, WorkItemStep::AgentBrief))
            == Some(WorkItemStepStatus::Pending);
        if !pending {
            return;
        }
        self.set_work_item_agent_step(job_id, WorkItemStepStatus::Running, None);
        if let Some(job) = self.work_items.job_mut(job_id) {
            job.agent_start = Some(AgentAttempt {
                started: now,
                next_attempt: now,
                pending: None,
            });
        }
        self.advance_work_item_agent(job_id, now);
    }

    /// Retries `agent.start` until the shell accepts it, then delivers the brief once
    /// the agent is ready.
    fn advance_work_item_agent(&mut self, job_id: u64, now: Instant) {
        let Some(job) = self.work_items.job(job_id) else {
            return;
        };
        if let Some(attempt) = &job.agent_start {
            if attempt.next_attempt <= now {
                let started = attempt.started;
                self.try_start_work_item_agent(job_id, started, now);
            }
        } else if job.brief.is_some() {
            self.advance_work_item_brief(job_id, now);
        }
        self.work_items.finish_job_if_done(job_id);
    }

    fn try_start_work_item_agent(&mut self, job_id: u64, started: Instant, now: Instant) {
        let Some(job) = self.work_items.job(job_id) else {
            return;
        };
        let Some(pane_id) = job.agent_pane_id.clone() else {
            return;
        };
        let hint = job.plan.agent_name_hint.clone();
        let kind = job.plan.layout.agent.clone();
        for suffix in 1..=MAX_AGENT_NAME_SUFFIX {
            let name = if suffix == 1 {
                hint.clone()
            } else {
                format!("{hint}-{suffix}")
            };
            match self.work_items_api(Method::AgentStart(AgentStartParams {
                name: name.clone(),
                kind: kind.clone(),
                pane_id: pane_id.clone(),
                args: Vec::new(),
                timeout_ms: None,
            })) {
                Ok(_) => {
                    if let Some(job) = self.work_items.job_mut(job_id) {
                        job.agent_start = None;
                        job.agent_name = Some(name);
                        job.brief = Some(AgentAttempt {
                            started: now,
                            next_attempt: now + AGENT_RETRY_INTERVAL,
                            pending: None,
                        });
                    }
                    return;
                }
                Err(err) if err.code == "agent_name_taken" => continue,
                Err(err)
                    if err.code == "agent_pane_busy"
                        && now.saturating_duration_since(started) < AGENT_READY_TIMEOUT =>
                {
                    if let Some(attempt) = self
                        .work_items
                        .job_mut(job_id)
                        .and_then(|job| job.agent_start.as_mut())
                    {
                        attempt.next_attempt = now + AGENT_RETRY_INTERVAL;
                    }
                    return;
                }
                Err(err) => {
                    self.fail_work_item_agent(job_id, err.message);
                    return;
                }
            }
        }
        self.fail_work_item_agent(
            job_id,
            format!("agent names {hint} to {hint}-{MAX_AGENT_NAME_SUFFIX} are taken"),
        );
    }

    fn fail_work_item_agent(&mut self, job_id: u64, message: String) {
        if let Some(job) = self.work_items.job_mut(job_id) {
            job.agent_start = None;
            job.brief = None;
        }
        self.set_work_item_agent_step(job_id, WorkItemStepStatus::Failed, Some(message));
    }

    fn advance_work_item_brief(&mut self, job_id: u64, now: Instant) {
        enum Next {
            Wait,
            Send { agent_name: String, text: String },
            Done,
            Fail(String),
        }
        let next = {
            let Some(job) = self.work_items.job_mut(job_id) else {
                return;
            };
            let agent_name = job.agent_name.clone();
            let text = job.plan.brief.clone();
            let Some(attempt) = job.brief.as_mut() else {
                return;
            };
            match attempt.pending.as_ref().map(|pending| pending.try_recv()) {
                Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {
                    attempt.next_attempt = now + BRIEF_POLL_INTERVAL;
                    Next::Wait
                }
                Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                    Next::Fail("agent prompt response was lost".into())
                }
                Some(Ok(response)) => {
                    attempt.pending = None;
                    match parse_response(&response) {
                        Ok(_) => Next::Done,
                        Err(err)
                            if err.code == "agent_not_ready"
                                && now.saturating_duration_since(attempt.started)
                                    < AGENT_READY_TIMEOUT =>
                        {
                            attempt.next_attempt = now + AGENT_RETRY_INTERVAL;
                            Next::Wait
                        }
                        Err(err) if err.code == "agent_not_ready" => Next::Fail(format!(
                            "agent did not become ready within {} s",
                            AGENT_READY_TIMEOUT.as_secs()
                        )),
                        Err(err) => Next::Fail(err.message),
                    }
                }
                None if attempt.next_attempt > now => Next::Wait,
                None => match agent_name {
                    Some(agent_name) => Next::Send { agent_name, text },
                    None => Next::Fail("agent name is unknown".into()),
                },
            }
        };
        match next {
            Next::Wait => {}
            Next::Done => {
                if let Some(job) = self.work_items.job_mut(job_id) {
                    job.brief = None;
                }
                self.set_work_item_agent_step(job_id, WorkItemStepStatus::Done, None);
            }
            Next::Fail(message) => self.fail_work_item_agent(job_id, message),
            Next::Send { agent_name, text } => {
                let (tx, rx) = std::sync::mpsc::channel();
                if let Some(attempt) = self
                    .work_items
                    .job_mut(job_id)
                    .and_then(|job| job.brief.as_mut())
                {
                    attempt.pending = Some(rx);
                    attempt.next_attempt = now + BRIEF_POLL_INTERVAL;
                }
                self.handle_deferred_agent_api_request(
                    Request {
                        id: "work-items".into(),
                        method: Method::AgentPrompt(AgentPromptParams {
                            target: agent_name,
                            text,
                            wait: None,
                        }),
                    },
                    tx,
                );
            }
        }
    }
}

fn unexpected_response(method: &str) -> ErrorBody {
    ErrorBody {
        code: "invalid_response".into(),
        message: format!("unexpected {method} response"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::api::schema::{
        EmptyParams, ErrorResponse, Method, Request, ResponseResult, SuccessResponse,
        WorkItemChooseParams, WorkItemInfo, WorkItemPhase,
    };
    use crate::app::{App, AppPolicy};
    use crate::work_items::test_support::{source_item, FakeSource};
    use crate::work_items::WorkItems;

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    fn request(method: Method) -> Request {
        Request {
            id: "test".into(),
            method,
        }
    }

    fn list(app: &mut App) -> Vec<WorkItemInfo> {
        let response =
            app.handle_api_request(request(Method::WorkItemList(EmptyParams::default())));
        let success: SuccessResponse = serde_json::from_str(&response).expect("list succeeds");
        let ResponseResult::WorkItemList { items, .. } = success.result else {
            panic!("expected work item list, got {response}");
        };
        items
    }

    /// Runs due tasks and drains events until `done` holds or two seconds pass.
    fn run_until(app: &mut App, mut done: impl FnMut(&mut App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            app.run_work_items_tasks(Instant::now());
            app.drain_all_internal_events();
            if done(app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not reached within 2 s: {:#?}", list(app));
    }

    #[test]
    fn default_config_schedules_nothing_and_reports_disabled() {
        let mut app = test_app();
        assert_eq!(app.work_items.next_deadline(), None);
        assert!(!app.run_work_items_tasks(Instant::now()));
        assert!(app.event_rx.try_recv().is_err());
        let response =
            app.handle_api_request(request(Method::WorkItemList(EmptyParams::default())));
        let error: ErrorResponse = serde_json::from_str(&response).expect("error response");
        assert_eq!(error.error.code, "work_items_disabled");
    }

    #[test]
    fn first_poll_runs_immediately_and_prepares_new_items() {
        let mut app = test_app();
        let source = FakeSource::with_items(vec![source_item("a")]);
        app.work_items = WorkItems::for_test(vec![source.clone()], Instant::now());
        run_until(&mut app, |app| {
            list(app).first().is_some_and(|item| item.summary.is_some())
        });
        let items = list(&mut app);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "fake:a");
        assert!(!items[0].seen);
        assert_eq!(items[0].phase, WorkItemPhase::Pending);
        assert_eq!(source.prepare_calls(), 1);
    }

    #[test]
    fn external_choice_awaits_until_the_source_drops_the_item() {
        let mut app = test_app();
        let source = FakeSource::with_items(vec![source_item("a")]);
        app.work_items = WorkItems::for_test(vec![source.clone() as Arc<_>], Instant::now());
        run_until(&mut app, |app| !list(app).is_empty());

        let response =
            app.handle_api_request(request(Method::WorkItemChoose(WorkItemChooseParams {
                item_id: "fake:a".into(),
                choice_id: "web".into(),
            })));
        assert!(
            serde_json::from_str::<SuccessResponse>(&response).is_ok(),
            "{response}"
        );
        let items = list(&mut app);
        assert_eq!(items[0].phase, WorkItemPhase::AwaitingExternal);
        assert!(items[0].seen);

        source.set_items(Vec::new());
        app.work_items.schedule_all_for_test(Instant::now());
        run_until(&mut app, |app| list(app).is_empty());
    }

    fn git(cwd: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args([
                "-c",
                "user.name=herdr",
                "-c",
                "user.email=herdr@example.test",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn api(app: &mut App, method: Method) -> Result<ResponseResult, String> {
        let response = app.handle_api_request(request(method));
        serde_json::from_str::<SuccessResponse>(&response)
            .map(|success| success.result)
            .map_err(|_| {
                serde_json::from_str::<ErrorResponse>(&response)
                    .map(|error| error.error.code)
                    .unwrap_or(response)
            })
    }

    struct ReviewRepo {
        root: std::path::PathBuf,
        repo: std::path::PathBuf,
    }

    impl ReviewRepo {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-work-items-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let repo = root.join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "--quiet"]);
            git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "base"]);
            git(
                &repo,
                &["commit", "--quiet", "--allow-empty", "-m", "change"],
            );
            git(&repo, &["update-ref", "refs/pull/1/head", "HEAD"]);
            git(&repo, &["reset", "--quiet", "--hard", "HEAD~1"]);
            Self { root, repo }
        }

        fn branch_exists(&self, branch: &str) -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args([
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ])
                .output()
                .expect("git")
                .status
                .success()
        }

        fn worktree_path(&self, branch: &str) -> Option<std::path::PathBuf> {
            let listing = git(&self.repo, &["worktree", "list", "--porcelain"]);
            let mut path = None;
            for line in listing.lines() {
                if let Some(found) = line.strip_prefix("worktree ") {
                    path = Some(std::path::PathBuf::from(found));
                } else if line == format!("branch refs/heads/{branch}") {
                    return path;
                }
            }
            None
        }
    }

    impl Drop for ReviewRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// App with a fake source whose "local" choice plans a worktree review of `repo`.
    fn provisioning_app(repo: &ReviewRepo) -> (App, Arc<FakeSource>) {
        use crate::work_items::source::{
            ProvisionPlan, WorkspaceLayout, WorkspaceSource, WorktreeSpec,
        };

        let mut app = test_app();
        app.state.default_shell = "/bin/sh".into();
        app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
        app.state.worktree_directory = repo.root.join("worktrees");
        let source = FakeSource::with_items(vec![source_item("1")]);
        *source.plan.lock().unwrap() = Some(ProvisionPlan {
            source: WorkspaceSource::Worktree(WorktreeSpec {
                repo_path: repo.repo.clone(),
                remote: repo.repo.display().to_string(),
                fetch_refspec: "+refs/pull/1/head:refs/herdr/pull/1".into(),
                base_ref: "refs/herdr/pull/1".into(),
                branch: "review/pr-1".into(),
            }),
            workspace_label: "#1 Title 1".into(),
            agent_name_hint: "review-1".into(),
            brief: "brief".into(),
            layout: WorkspaceLayout {
                agent: String::new(),
                editor_command: "true".into(),
                lazygit_command: "true".into(),
                diff_command: String::new(),
            },
            delete_branch: true,
        });
        app.work_items = WorkItems::for_test(vec![source.clone() as Arc<_>], Instant::now());
        run_until(&mut app, |app| !list(app).is_empty());
        (app, source)
    }

    fn choose_local(app: &mut App) -> Result<ResponseResult, String> {
        api(
            app,
            Method::WorkItemChoose(WorkItemChooseParams {
                item_id: "fake:1".into(),
                choice_id: "local".into(),
            }),
        )
    }

    fn provision(app: &mut App) -> String {
        choose_local(app).expect("local choice accepted");
        run_until(app, |app| {
            list(app)[0]
                .provisioning
                .as_ref()
                .is_some_and(|provisioning| provisioning.finished)
        });
        list(app)[0]
            .workspace_id
            .clone()
            .expect("item owns a workspace")
    }

    fn wait_for(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not reached within 2 s");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_choice_creates_a_herdr_worktree_on_a_review_branch() {
        use crate::api::schema::{TabListParams, WorkItemStepStatus};

        let repo = ReviewRepo::new("provision");
        let (mut app, _source) = provisioning_app(&repo);
        let workspace_id = provision(&mut app);

        let item = list(&mut app).remove(0);
        let statuses: Vec<_> = item
            .provisioning
            .as_ref()
            .unwrap()
            .steps
            .iter()
            .map(|step| step.status)
            .collect();
        assert_eq!(
            statuses,
            vec![WorkItemStepStatus::Done, WorkItemStepStatus::Skipped]
        );
        assert_eq!(item.phase, WorkItemPhase::Local);
        let ws_idx = app
            .parse_workspace_id(&workspace_id)
            .expect("workspace exists");
        assert!(
            app.state.workspaces[ws_idx]
                .worktree_space()
                .is_some_and(|space| space.is_linked_worktree),
            "workspace is registered as a Herdr worktree"
        );
        let Ok(ResponseResult::TabList { tabs }) = api(
            &mut app,
            Method::TabList(TabListParams {
                workspace_id: Some(workspace_id.clone()),
            }),
        ) else {
            panic!("tab list");
        };
        let labels: Vec<_> = tabs.iter().map(|tab| tab.label.as_str()).collect();
        assert_eq!(labels, vec!["shell", "editor", "lazygit"]);
        let checkout = repo.worktree_path("review/pr-1").expect("review worktree");
        assert_eq!(
            git(&checkout, &["rev-parse", "HEAD"]),
            git(&repo.repo, &["rev-parse", "refs/pull/1/head"])
        );
        assert_eq!(
            choose_local(&mut app).expect_err("second choice rejected"),
            "work_item_already_provisioned"
        );
        crate::app::api::test_support::shutdown_test_runtimes(&mut app);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn removing_the_review_worktree_deletes_its_branch() {
        let repo = ReviewRepo::new("remove-branch");
        let (mut app, _source) = provisioning_app(&repo);
        let workspace_id = provision(&mut app);
        assert!(repo.branch_exists("review/pr-1"));

        let (tx, rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_worktree_api_request(
            request(Method::WorktreeRemove(
                crate::api::schema::WorktreeRemoveParams {
                    workspace_id,
                    force: false,
                    trust_repository: false,
                }
            )),
            tx,
        ));
        run_until(&mut app, |_| rx.try_recv().is_ok());
        wait_for(|| !repo.branch_exists("review/pr-1"));
        let item = list(&mut app).remove(0);
        assert_eq!(item.phase, WorkItemPhase::Pending);
        assert_eq!(item.workspace_id, None);
        crate::app::api::test_support::shutdown_test_runtimes(&mut app);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolution_removes_the_worktree_when_the_workflow_asks_for_it() {
        let repo = ReviewRepo::new("resolve-remove");
        let (mut app, source) = provisioning_app(&repo);
        provision(&mut app);
        source
            .remove_on_resolved
            .store(true, std::sync::atomic::Ordering::SeqCst);

        source.set_items(Vec::new());
        app.work_items.schedule_all_for_test(Instant::now());
        run_until(&mut app, |app| list(app).is_empty());
        assert_eq!(repo.worktree_path("review/pr-1"), None);
        wait_for(|| !repo.branch_exists("review/pr-1"));
        crate::app::api::test_support::shutdown_test_runtimes(&mut app);
    }
}
