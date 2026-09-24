//! App-thread driver for work items: schedules polls, preparation and local
//! provisioning on background threads and applies their results.
//!
//! Provisioning drives the workspace through the public API methods so it stays
//! decoupled from internal creation helpers.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::{App, AppPolicy};
use crate::api::schema::{
    AgentPromptParams, AgentStartParams, ErrorBody, ErrorResponse, Method, PaneSendInputParams,
    Request, ResponseResult, SuccessResponse, TabCreateParams, TabRenameParams, WorkItemStep,
    WorkItemStepStatus, WorkspaceCreateParams,
};
use crate::events::AppEvent;
use crate::work_items::provision::{self, AgentAttempt};
use crate::work_items::{StorePolicy, WorkItemNotice, WorkItemsEvent};

/// How long the agent may take to accept `agent.start` and then its brief.
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(90);
const AGENT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const BRIEF_POLL_INTERVAL: Duration = Duration::from_millis(250);
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
            self.advance_work_item_agent(job_id, now);
        }
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
                self.work_item_checkout_finished(job_id, result, now);
            }
            WorkItemsEvent::DependenciesFinished { job_id, result } => {
                self.work_item_dependencies_finished(job_id, result);
            }
            WorkItemsEvent::ServerProbeFinished { job_id, result } => {
                let (status, detail) = match result {
                    Ok(()) => (WorkItemStepStatus::Done, None),
                    Err(err) => (WorkItemStepStatus::Failed, Some(err)),
                };
                self.work_items.update_progress(job_id, |progress| {
                    provision::set_step(progress, WorkItemStep::Server, status, detail)
                });
                self.work_items.finish_job_if_done(job_id);
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
            .provision_plan(&item, &self.state.worktree_directory)
            .map_err(|message| ("work_item_unavailable", message))?;
        let spec = plan.checkout.clone();
        let agent = self.work_items.config().workspace.agent.clone();
        let job_id = self
            .work_items
            .start_job(key, plan, &agent)
            .map_err(|_| ("work_item_not_found", format!("unknown work item {key}")))?;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = provision::checkout(&spec);
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

    fn work_item_checkout_finished(
        &mut self,
        job_id: u64,
        result: Result<(), String>,
        now: Instant,
    ) {
        if let Err(err) = result {
            self.work_items
                .update_progress(job_id, |progress| provision::fail_checkout(progress, err));
            self.work_items.finish_job_if_done(job_id);
            return;
        }
        self.work_items.update_progress(job_id, |progress| {
            provision::set_step(
                progress,
                WorkItemStep::Checkout,
                WorkItemStepStatus::Done,
                None,
            )
        });
        if let Err(err) = self.create_work_item_workspace(job_id) {
            let detail = format!("workspace setup failed: {}", err.message);
            self.work_items.update_progress(job_id, |progress| {
                provision::end_unfinished(progress, WorkItemStepStatus::Failed, &detail)
            });
            self.work_items.finish_job_if_done(job_id);
            return;
        }
        let dependencies_pending = self
            .work_items
            .progress(job_id)
            .and_then(|progress| provision::status(progress, WorkItemStep::Dependencies))
            == Some(WorkItemStepStatus::Pending);
        if dependencies_pending {
            self.start_work_item_dependencies(job_id);
        } else {
            self.start_work_item_server(job_id);
        }
        self.start_work_item_agent(job_id, now);
        self.work_items.finish_job_if_done(job_id);
    }

    /// Creates the workspace and its tabs for a checked-out job.
    fn create_work_item_workspace(&mut self, job_id: u64) -> Result<(), ErrorBody> {
        let Some(job) = self.work_items.job(job_id) else {
            return Ok(());
        };
        let plan = job.plan.clone();
        let layout: crate::config::WorkItemWorkspaceConfig =
            self.work_items.config().workspace.clone();
        let cwd = plan.checkout.checkout_path.display().to_string();
        let ResponseResult::WorkspaceCreated {
            workspace,
            tab,
            root_pane,
        } = self.work_items_api(Method::WorkspaceCreate(WorkspaceCreateParams {
            source_workspace_id: None,
            cwd: Some(cwd.clone()),
            focus: false,
            label: Some(plan.workspace_label.clone()),
            env: Default::default(),
        }))?
        else {
            return Err(unexpected_response("workspace.create"));
        };
        self.work_items
            .link_workspace(job_id, &workspace.workspace_id);
        let first_tab = if layout.agent.is_empty() {
            "shell"
        } else {
            "agent"
        };
        self.work_items_api(Method::TabRename(TabRenameParams {
            tab_id: tab.tab_id,
            label: first_tab.into(),
        }))?;
        if let Some(job) = self.work_items.job_mut(job_id) {
            job.agent_pane_id = Some(root_pane.pane_id);
        }
        for (label, command) in [
            ("editor", layout.editor_command.as_str()),
            ("lazygit", layout.lazygit_command.as_str()),
        ] {
            if command.is_empty() {
                continue;
            }
            let pane_id = self.create_work_item_tab(&workspace.workspace_id, &cwd, label)?;
            self.work_items_api(Method::PaneSendInput(PaneSendInputParams {
                pane_id,
                text: command.to_string(),
                keys: vec!["enter".into()],
            }))?;
        }
        if plan.server.is_some() {
            let pane_id = self.create_work_item_tab(&workspace.workspace_id, &cwd, "server")?;
            if let Some(job) = self.work_items.job_mut(job_id) {
                job.server_pane_id = Some(pane_id);
            }
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

    fn start_work_item_dependencies(&mut self, job_id: u64) {
        let Some(job) = self.work_items.job(job_id) else {
            return;
        };
        let Some(command) = job.plan.install_command.clone() else {
            return;
        };
        let cwd = job.plan.checkout.checkout_path.clone();
        self.work_items.update_progress(job_id, |progress| {
            provision::set_step(
                progress,
                WorkItemStep::Dependencies,
                WorkItemStepStatus::Running,
                None,
            )
        });
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = provision::install(&command, &cwd);
            send_event(
                &event_tx,
                WorkItemsEvent::DependenciesFinished { job_id, result },
            );
        });
    }

    fn work_item_dependencies_finished(&mut self, job_id: u64, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.work_items.update_progress(job_id, |progress| {
                    provision::set_step(
                        progress,
                        WorkItemStep::Dependencies,
                        WorkItemStepStatus::Done,
                        None,
                    )
                });
                self.start_work_item_server(job_id);
            }
            Err(err) => self.work_items.update_progress(job_id, |progress| {
                provision::fail_dependencies(progress, err)
            }),
        }
        self.work_items.finish_job_if_done(job_id);
    }

    fn start_work_item_server(&mut self, job_id: u64) {
        let Some(job) = self.work_items.job(job_id) else {
            return;
        };
        let (Some(server), Some(pane_id)) = (job.plan.server.clone(), job.server_pane_id.clone())
        else {
            return;
        };
        if let Err(err) = self.work_items_api(Method::PaneSendInput(PaneSendInputParams {
            pane_id,
            text: server.command,
            keys: vec!["enter".into()],
        })) {
            self.work_items.update_progress(job_id, |progress| {
                provision::set_step(
                    progress,
                    WorkItemStep::Server,
                    WorkItemStepStatus::Failed,
                    Some(err.message),
                )
            });
            return;
        }
        let Some(port) = server.port else {
            self.work_items.update_progress(job_id, |progress| {
                provision::set_step(
                    progress,
                    WorkItemStep::Server,
                    WorkItemStepStatus::Done,
                    Some("started".into()),
                )
            });
            return;
        };
        self.work_items.update_progress(job_id, |progress| {
            provision::set_step(
                progress,
                WorkItemStep::Server,
                WorkItemStepStatus::Running,
                None,
            )
        });
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = provision::probe(port);
            send_event(
                &event_tx,
                WorkItemsEvent::ServerProbeFinished { job_id, result },
            );
        });
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
        let kind = self.work_items.config().workspace.agent.clone();
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
        app.run_work_items_tasks(Instant::now());
        while Instant::now() < deadline {
            app.drain_all_internal_events();
            if done(app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not reached within 2 s");
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

    #[cfg(unix)]
    #[tokio::test]
    async fn local_choice_provisions_a_worktree_workspace_owned_by_the_item() {
        use crate::api::schema::{TabListParams, WorkItemStepStatus, WorkspaceCloseParams};
        use crate::work_items::source::{CheckoutSpec, ProvisionPlan};

        let root = std::env::temp_dir().join(format!(
            "herdr-work-items-provision-{}-{}",
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
        let checkout_path = root.join("worktrees").join("repo").join("pr-1");

        let mut app = test_app();
        app.state.default_shell = "/bin/sh".into();
        app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
        let source = FakeSource::with_items(vec![source_item("1")]);
        *source.plan.lock().unwrap() = Some(ProvisionPlan {
            checkout: CheckoutSpec {
                repo_path: repo.clone(),
                remote: repo.display().to_string(),
                fetch_refspec: "+refs/pull/1/head:refs/herdr/pull/1".into(),
                checkout_ref: "refs/herdr/pull/1".into(),
                checkout_path: checkout_path.clone(),
            },
            workspace_label: "#1 Title 1".into(),
            agent_name_hint: "review-1".into(),
            brief: "brief".into(),
            install_command: None,
            server: None,
            server_skip_reason: "no front-end changes".into(),
        });
        app.work_items = WorkItems::for_test(vec![source.clone() as Arc<_>], Instant::now());
        app.work_items
            .set_workspace_layout_for_test(crate::config::WorkItemWorkspaceConfig {
                agent: String::new(),
                editor_command: "true".into(),
                lazygit_command: "true".into(),
            });
        run_until(&mut app, |app| !list(app).is_empty());

        let choose = || {
            Method::WorkItemChoose(WorkItemChooseParams {
                item_id: "fake:1".into(),
                choice_id: "local".into(),
            })
        };
        api(&mut app, choose()).expect("local choice accepted");
        run_until(&mut app, |app| {
            list(app)[0]
                .provisioning
                .as_ref()
                .is_some_and(|provisioning| provisioning.finished)
        });

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
            vec![
                WorkItemStepStatus::Done,
                WorkItemStepStatus::Skipped,
                WorkItemStepStatus::Skipped,
                WorkItemStepStatus::Skipped,
            ]
        );
        assert_eq!(item.phase, WorkItemPhase::Local);
        let workspace_id = item.workspace_id.clone().expect("item owns a workspace");
        let Ok(ResponseResult::WorkspaceList { workspaces }) =
            api(&mut app, Method::WorkspaceList(EmptyParams::default()))
        else {
            panic!("workspace list");
        };
        let workspace = workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
            .expect("provisioned workspace exists");
        assert_eq!(workspace.label, "#1 Title 1");
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
        assert_eq!(
            git(&checkout_path, &["rev-parse", "HEAD"]),
            git(&repo, &["rev-parse", "refs/pull/1/head"])
        );

        assert_eq!(
            api(&mut app, choose()).expect_err("second choice rejected"),
            "work_item_already_provisioned"
        );

        api(
            &mut app,
            Method::WorkspaceClose(WorkspaceCloseParams {
                workspace_id,
                close_group: false,
            }),
        )
        .expect("workspace closes");
        let item = list(&mut app).remove(0);
        assert_eq!(item.phase, WorkItemPhase::Pending);
        assert_eq!(item.workspace_id, None);

        crate::app::api::test_support::shutdown_test_runtimes(&mut app);
        let _ = std::fs::remove_dir_all(&root);
    }
}
