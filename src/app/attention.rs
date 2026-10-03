//! App-thread driver for attention: gathers each agent pane's signals, has work items
//! combine them with the items' own state, and publishes `attention.changed`.
//!
//! Worked out only when something it depends on changed (an agent's status, a pane or
//! workspace closing, the items themselves) or a finished turn's quiet period ends, never
//! per render or per byte of output.

use std::collections::HashMap;
use std::time::{Instant, SystemTime};

use super::App;
use crate::api::schema::{AgentAttentionInfo, EventData, EventEnvelope, EventKind};
use crate::work_items::attention::{
    self, AgentSignal, AgentVerdict, Candidate, Subject, Transition,
};

/// What the agents mean for you right now.
struct AgentVerdicts {
    /// The folded verdict of each item workspace's agents, by workspace id.
    by_workspace: HashMap<String, AgentVerdict>,
    /// The needs of agents outside every item, by pane id.
    loose: Vec<(String, Candidate)>,
    /// The earliest time a verdict may change on its own.
    recheck_at: Option<Instant>,
}

impl App {
    /// Records that something attention depends on changed, when `event` is such a change.
    pub(super) fn note_attention_event(&mut self, event: &EventData) {
        if matches!(
            event,
            EventData::PaneAgentStatusChanged { .. }
                | EventData::PaneAgentDetected { .. }
                | EventData::PaneClosed { .. }
                | EventData::PaneExited { .. }
                | EventData::PaneMoved { .. }
                | EventData::WorkspaceClosed { .. }
        ) {
            self.attention_schedule.mark_dirty();
        }
    }

    /// When a finished turn's quiet period ends and attention must be worked out again.
    pub(crate) fn attention_deadline(&self) -> Option<Instant> {
        self.attention_schedule.recheck_at()
    }

    /// Works out what needs you, when due, and returns what changed since the last time.
    /// Items whose attention changed get a new work-item revision.
    pub(super) fn update_attention(&mut self, now: Instant) -> Vec<Transition> {
        if !self.work_items.is_enabled()
            || !self
                .attention_schedule
                .is_due(now, self.work_items.revision())
        {
            return Vec::new();
        }
        let verdicts = self.agent_verdicts(now);
        let transitions = self.work_items.update_attention(
            &verdicts.by_workspace,
            verdicts.loose,
            crate::work_items::unix_now(),
        );
        self.attention_schedule
            .done(self.work_items.revision(), verdicts.recheck_at);
        transitions
    }

    pub(super) fn emit_attention_transitions(&mut self, transitions: Vec<Transition>) {
        for transition in transitions {
            let (item_id, pane_id) = match transition.subject {
                Subject::Item(item_id) => (Some(item_id), None),
                Subject::Pane(pane_id) => (None, Some(pane_id)),
            };
            self.emit_event(EventEnvelope {
                event: EventKind::AttentionChanged,
                data: EventData::AttentionChanged {
                    item_id,
                    pane_id,
                    workspace_id: transition.workspace_id,
                    title: transition.title,
                    attention: transition.attention,
                },
            });
        }
    }

    fn agent_verdicts(&self, now: Instant) -> AgentVerdicts {
        // Read once, so that every agent's command has run for the same time.
        let wall_now = SystemTime::now();
        let item_workspaces = self.work_items.item_workspace_ids();
        let mut by_workspace = HashMap::new();
        let mut loose = Vec::new();
        let mut recheck_at = None;
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            let linked = item_workspaces.contains(ws.id.as_str());
            let mut verdicts = Vec::new();
            for signal in self.agent_signals(ws_idx) {
                let verdict = attention::agent_verdict(&signal, now, wall_now);
                recheck_at = attention::earliest(recheck_at, verdict.recheck_at);
                if linked {
                    verdicts.push(verdict);
                } else if let Some(need) = verdict.need {
                    let label =
                        ws.display_name_from(&self.state.terminals, &self.terminal_runtimes);
                    loose.push((
                        signal.pane_id,
                        Candidate {
                            need,
                            title: format!("{} \u{b7} {label}", signal.agent),
                            workspace_id: Some(ws.id.clone()),
                        },
                    ));
                }
            }
            if linked {
                by_workspace.insert(ws.id.clone(), attention::fold_verdicts(verdicts));
            }
        }
        AgentVerdicts {
            by_workspace,
            loose,
            recheck_at,
        }
    }

    /// What the agent in the pane `pane_id` of workspace `ws_idx`, attached to `terminal_id`,
    /// means for you; `None` when no agent runs there.
    fn agent_signal(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        terminal_id: &crate::terminal::TerminalId,
    ) -> Option<AgentSignal<'_>> {
        let terminal = self.state.terminals.get(terminal_id)?;
        let agent = terminal.effective_agent_label()?;
        let public_pane_id = self.public_pane_id(ws_idx, pane_id)?;
        Some(AgentSignal {
            pane_id: public_pane_id,
            agent,
            state: terminal.state,
            blocked_message: terminal
                .hook_authority
                .as_ref()
                .filter(|authority| authority.state == crate::detect::AgentState::Blocked)
                .and_then(|authority| authority.message.as_deref()),
            turn_finished_at: terminal.turn_finished_at,
            dismissed_turn: terminal.dismissed_turn_at,
            last_input_at: self
                .terminal_runtimes
                .get(terminal_id)
                .and_then(|runtime| runtime.last_user_input_at()),
            running_command: self.running_commands.get(terminal_id).cloned(),
            stuck_after: self.work_items.stuck_after(),
            working_since: terminal.working_since,
        })
    }

    /// Every pane of workspace `ws_idx` running a detected agent.
    fn agent_signals(&self, ws_idx: usize) -> Vec<AgentSignal<'_>> {
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Vec::new();
        };
        ws.tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .filter_map(|(pane_id, pane)| {
                self.agent_signal(ws_idx, *pane_id, &pane.attached_terminal_id)
            })
            .collect()
    }

    /// Whether the agent in the pane `pane_id` of workspace `ws_idx`, attached to
    /// `terminal_id`, has a finished turn that needs you at `now`: the turn `agent.dismiss`
    /// deals with. Not while the agent works or waits on you, within the quiet period after
    /// its turn, nor once the turn was dealt with.
    pub(super) fn agent_turn_needs_you(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        terminal_id: &crate::terminal::TerminalId,
        now: Instant,
    ) -> bool {
        self.agent_signal(ws_idx, pane_id, terminal_id)
            .and_then(|signal| attention::agent_verdict(&signal, now, SystemTime::now()).need)
            .is_some_and(|need| need.kind == crate::api::schema::AttentionKind::Finished)
    }

    /// Agents in panes outside every item's workspace, with what they need from you.
    pub(super) fn agents_outside_items(&self) -> Vec<AgentAttentionInfo> {
        let item_workspaces = self.work_items.item_workspace_ids();
        let mut agents = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            if item_workspaces.contains(ws.id.as_str()) {
                continue;
            }
            let mut label = None;
            let mut panes: Vec<_> = ws.tabs.iter().flat_map(|tab| tab.panes.iter()).collect();
            panes.sort_by_key(|(pane_id, _)| ws.public_pane_number(**pane_id));
            for (pane_id, pane) in panes {
                let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id) else {
                    continue;
                };
                let Some(agent) = terminal.effective_agent_label() else {
                    continue;
                };
                let Some(public_pane_id) = self.public_pane_id(ws_idx, *pane_id) else {
                    continue;
                };
                let workspace_label = label
                    .get_or_insert_with(|| {
                        ws.display_name_from(&self.state.terminals, &self.terminal_runtimes)
                    })
                    .clone();
                agents.push(AgentAttentionInfo {
                    attention: self
                        .work_items
                        .attention(&Subject::Pane(public_pane_id.clone()))
                        .cloned(),
                    pane_id: public_pane_id,
                    workspace_id: ws.id.clone(),
                    workspace_label,
                    agent: agent.to_string(),
                    agent_status: super::api_helpers::pane_agent_status(terminal.state, pane.seen),
                });
            }
        }
        agents
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    use crate::api::schema::{
        AgentTarget, AttentionKind, EmptyParams, ErrorResponse, EventData, Method, PaneAgentState,
        PaneReportAgentParams, Request, ResponseResult, SuccessResponse, WorkItemLinkParams,
    };
    use crate::app::running_commands::test_support;
    use crate::app::{App, AppPolicy};
    use crate::work_items::attention::RunningCommand;
    use crate::work_items::test_support::{source_item, FakeSource};
    use crate::work_items::WorkItems;

    fn request(method: Method) -> Request {
        Request {
            id: "test".into(),
            method,
        }
    }

    fn list(
        app: &mut App,
    ) -> (
        Vec<crate::api::schema::WorkItemInfo>,
        Vec<crate::api::schema::AgentAttentionInfo>,
    ) {
        let response =
            app.handle_api_request(request(Method::WorkItemList(EmptyParams::default())));
        let success: SuccessResponse = serde_json::from_str(&response).expect("list succeeds");
        let ResponseResult::WorkItemList { items, agents, .. } = success.result else {
            panic!("expected work item list, got {response}");
        };
        (items, agents)
    }

    fn report(app: &mut App, ws_idx: usize, state: PaneAgentState, message: Option<&str>) {
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        let pane_public = app.public_pane_id(ws_idx, pane_id).expect("pane");
        app.handle_api_request(request(Method::PaneReportAgent(PaneReportAgentParams {
            pane_id: pane_public,
            source: "test".into(),
            agent: "claude".into(),
            state,
            message: message.map(str::to_string),
            seq: None,
            agent_session_id: None,
            agent_session_path: None,
            resume_argv: None,
        })));
        app.run_work_items_tasks(Instant::now());
    }

    /// `(item_id, pane_id, kind)` of every `attention.changed` event so far.
    fn attention_events(
        hub: &crate::api::EventHub,
    ) -> Vec<(Option<String>, Option<String>, Option<AttentionKind>)> {
        hub.events_after(0)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                EventData::AttentionChanged {
                    item_id,
                    pane_id,
                    attention,
                    ..
                } => Some((item_id, pane_id, attention.map(|attention| attention.kind))),
                _ => None,
            })
            .collect()
    }

    /// An app with one fake item, `fake:a`, polled in, and two workspaces with a terminal
    /// each: `a` and `b`.
    fn app_with_item() -> (App, crate::api::EventHub) {
        let hub = crate::api::EventHub::default();
        let mut app = App::new(
            &crate::config::Config::default(),
            AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            hub.clone(),
        );
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("a"),
            crate::workspace::Workspace::test_new("b"),
        ];
        app.state.ensure_test_terminals();
        let source = FakeSource::with_items(vec![source_item("a")]);
        app.work_items = WorkItems::for_test(vec![source as Arc<_>], Instant::now());
        let deadline = Instant::now() + Duration::from_secs(2);
        while list(&mut app).0.is_empty() {
            assert!(Instant::now() < deadline, "item never arrived");
            app.run_work_items_tasks(Instant::now());
            app.drain_all_internal_events();
            std::thread::sleep(Duration::from_millis(10));
        }
        (app, hub)
    }

    #[test]
    fn item_needs_you_when_new_then_when_its_agent_blocks_and_not_while_it_works() {
        let (mut app, hub) = app_with_item();
        let item_id = Some("fake:a".to_string());
        assert_eq!(
            list(&mut app).0[0].attention.as_ref().map(|a| a.kind),
            Some(AttentionKind::New)
        );

        let workspace_id = app.state.workspaces[0].id.clone();
        app.handle_api_request(request(Method::WorkItemLink(WorkItemLinkParams {
            item_id: "fake:a".into(),
            workspace_id,
        })));
        assert_eq!(list(&mut app).0[0].attention, None);

        report(
            &mut app,
            0,
            PaneAgentState::Blocked,
            Some("Allow running bin/rails db:migrate?"),
        );
        let attention = list(&mut app).0[0].attention.clone().expect("blocked");
        assert_eq!(attention.kind, AttentionKind::Blocked);
        assert_eq!(attention.reason, "Allow running bin/rails db:migrate?");
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        assert_eq!(attention.pane_id, app.public_pane_id(0, pane_id));

        report(&mut app, 0, PaneAgentState::Working, None);
        assert_eq!(list(&mut app).0[0].attention, None);
        // The agent is in the item's workspace, so it is not listed on its own.
        assert!(list(&mut app).1.is_empty());

        assert_eq!(
            attention_events(&hub),
            vec![
                (item_id.clone(), None, Some(AttentionKind::New)),
                (item_id.clone(), None, None),
                (item_id.clone(), None, Some(AttentionKind::Blocked)),
                (item_id, None, None),
            ]
        );
    }

    #[test]
    fn blocked_agent_outside_every_item_is_listed_and_reported_by_pane() {
        let (mut app, hub) = app_with_item();
        report(&mut app, 1, PaneAgentState::Blocked, Some("Allow?"));

        let (_, agents) = list(&mut app);
        let pane_id = app.state.workspaces[1].tabs[0].root_pane;
        let pane_public = app.public_pane_id(1, pane_id);
        assert_eq!(agents.len(), 1);
        assert_eq!(Some(&agents[0].pane_id), pane_public.as_ref());
        assert_eq!(agents[0].workspace_id, app.state.workspaces[1].id);
        let attention = agents[0].attention.as_ref().expect("blocked");
        assert_eq!(attention.kind, AttentionKind::Blocked);
        assert_eq!(attention.reason, "Allow?");

        // Answered: the agent is idle, and its finished turn waits for the quiet period.
        report(&mut app, 1, PaneAgentState::Idle, None);
        assert_eq!(list(&mut app).1[0].attention, None);
        assert!(app.attention_deadline().is_some());

        let pane_events: Vec<_> = attention_events(&hub)
            .into_iter()
            .filter(|(item_id, _, _)| item_id.is_none())
            .collect();
        assert_eq!(
            pane_events,
            vec![
                (None, pane_public.clone(), Some(AttentionKind::Blocked)),
                (None, pane_public, None),
            ]
        );
    }

    /// Reports that the agent of workspace `ws_idx` finished a turn `minutes` minutes ago, so
    /// the turn already needs you.
    fn finish_turn_minutes_ago(app: &mut App, ws_idx: usize, minutes: u64) {
        report(app, ws_idx, PaneAgentState::Working, None);
        report(app, ws_idx, PaneAgentState::Idle, None);
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[ws_idx].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal")
            .turn_finished_at = Instant::now().checked_sub(Duration::from_secs(minutes * 60));
        app.attention_schedule.mark_dirty();
        app.sync_work_item_events();
    }

    fn dismiss(app: &mut App, ws_idx: usize) -> String {
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        let target = app.public_pane_id(ws_idx, pane_id).expect("pane");
        app.handle_api_request(request(Method::AgentDismiss(AgentTarget { target })))
    }

    fn is_ok(response: &str) -> bool {
        serde_json::from_str::<SuccessResponse>(response)
            .is_ok_and(|success| success.result == ResponseResult::Ok {})
    }

    fn error_code(response: &str) -> Option<String> {
        serde_json::from_str::<ErrorResponse>(response)
            .ok()
            .map(|error| error.error.code)
    }

    #[test]
    fn dismissing_a_finished_turn_clears_the_attention_of_an_agent_in_an_items_workspace() {
        let (mut app, hub) = app_with_item();
        let workspace_id = app.state.workspaces[0].id.clone();
        app.handle_api_request(request(Method::WorkItemLink(WorkItemLinkParams {
            item_id: "fake:a".into(),
            workspace_id,
        })));
        finish_turn_minutes_ago(&mut app, 0, 5);
        let kind = |app: &mut App| {
            list(app).0[0]
                .attention
                .as_ref()
                .map(|attention| attention.kind)
        };
        assert_eq!(kind(&mut app), Some(AttentionKind::Finished));

        assert!(is_ok(&dismiss(&mut app, 0)));
        assert_eq!(kind(&mut app), None);
        assert_eq!(
            attention_events(&hub).last(),
            Some(&(Some("fake:a".to_string()), None, None))
        );
    }

    #[test]
    fn dismissing_a_finished_turn_clears_the_attention_of_an_agent_outside_every_item() {
        let (mut app, hub) = app_with_item();
        finish_turn_minutes_ago(&mut app, 1, 5);
        let pane_id = app.state.workspaces[1].tabs[0].root_pane;
        let pane = app.public_pane_id(1, pane_id);
        let kind = |app: &mut App| {
            list(app).1[0]
                .attention
                .as_ref()
                .map(|attention| attention.kind)
        };
        assert_eq!(kind(&mut app), Some(AttentionKind::Finished));

        assert!(is_ok(&dismiss(&mut app, 1)));
        assert_eq!(kind(&mut app), None);
        assert_eq!(attention_events(&hub).last(), Some(&(None, pane, None)));
    }

    #[test]
    fn a_blocked_agent_cannot_be_dismissed_because_it_must_be_answered() {
        let (mut app, _hub) = app_with_item();
        report(&mut app, 1, PaneAgentState::Blocked, Some("Allow?"));

        assert_eq!(
            error_code(&dismiss(&mut app, 1)).as_deref(),
            Some("agent_blocked")
        );
        let attention = list(&mut app).1[0]
            .attention
            .clone()
            .expect("still blocked");
        assert_eq!(attention.kind, AttentionKind::Blocked);
    }

    #[test]
    fn dismissing_a_working_agent_changes_nothing() {
        let (mut app, _hub) = app_with_item();
        // Working: there is no finished turn yet, and the one it is on is not dismissed ahead.
        report(&mut app, 1, PaneAgentState::Working, None);
        assert!(is_ok(&dismiss(&mut app, 1)));
        finish_turn_minutes_ago(&mut app, 1, 5);
        let attention = list(&mut app).1[0].attention.clone().expect("finished");
        assert_eq!(attention.kind, AttentionKind::Finished);
    }

    #[test]
    fn dismissing_a_turn_that_does_not_need_you_yet_changes_nothing() {
        let (mut app, _hub) = app_with_item();
        // The turn just ended: it needs you only once the pane was left alone for the quiet
        // period, so there is nothing to deal with yet.
        finish_turn_minutes_ago(&mut app, 1, 0);
        assert_eq!(list(&mut app).1[0].attention, None);
        assert!(is_ok(&dismiss(&mut app, 1)));

        // Nothing was dismissed, so the turn needs you once that period is over.
        let quiet_period = crate::work_items::attention::FINISHED_QUIET_PERIOD;
        app.update_attention(Instant::now() + quiet_period + Duration::from_secs(1));
        let attention = list(&mut app).1[0].attention.clone().expect("finished");
        assert_eq!(attention.kind, AttentionKind::Finished);
    }

    #[test]
    fn dismissing_an_unknown_pane_is_not_found() {
        let (mut app, _hub) = app_with_item();
        let unknown = app.handle_api_request(request(Method::AgentDismiss(AgentTarget {
            target: "w9:p9".into(),
        })));
        assert_eq!(error_code(&unknown).as_deref(), Some("agent_not_found"));
    }

    fn terminal_of(app: &App, ws_idx: usize) -> crate::terminal::TerminalId {
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        app.state.workspaces[ws_idx].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone()
    }

    /// Reports the agent of workspace `ws_idx` working, with its transcript showing `command`
    /// running for `minutes` minutes. The agent has been working a minute longer.
    fn run_command_for_minutes(app: &mut App, ws_idx: usize, command: &str, minutes: u64) {
        report(app, ws_idx, PaneAgentState::Working, None);
        let terminal_id = terminal_of(app, ws_idx);
        let ran = Duration::from_secs(minutes * 60);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal")
            .working_since = Instant::now().checked_sub(ran + Duration::from_secs(60));
        app.running_commands.insert(
            terminal_id,
            RunningCommand {
                call_id: "toolu_1".into(),
                started_at: SystemTime::now() - ran,
                label: command.into(),
            },
        );
        app.attention_schedule.mark_dirty();
        app.sync_work_item_events();
    }

    #[test]
    fn an_agent_in_an_items_workspace_needs_you_once_one_command_ran_too_long_and_not_after() {
        let (mut app, hub) = app_with_item();
        let item_id = Some("fake:a".to_string());
        let workspace_id = app.state.workspaces[0].id.clone();
        app.handle_api_request(request(Method::WorkItemLink(WorkItemLinkParams {
            item_id: "fake:a".into(),
            workspace_id,
        })));

        run_command_for_minutes(&mut app, 0, "cargo test --all", 11);
        let attention = list(&mut app).0[0].attention.clone().expect("stuck");
        assert_eq!(attention.kind, AttentionKind::Stuck);
        assert_eq!(
            attention.reason,
            "`cargo test --all` has been running for over 10 min"
        );
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        assert_eq!(attention.pane_id, app.public_pane_id(0, pane_id));
        // The agent is in the item's workspace, so it is not listed on its own.
        assert!(list(&mut app).1.is_empty());

        report(&mut app, 0, PaneAgentState::Idle, None);
        assert_eq!(list(&mut app).0[0].attention, None);

        assert_eq!(
            attention_events(&hub),
            vec![
                (item_id.clone(), None, Some(AttentionKind::New)),
                (item_id.clone(), None, None),
                (item_id.clone(), None, Some(AttentionKind::Stuck)),
                (item_id, None, None),
            ]
        );
    }

    #[test]
    fn an_agent_outside_every_item_is_stuck_by_the_same_rule_and_reported_by_pane() {
        let (mut app, hub) = app_with_item();
        run_command_for_minutes(&mut app, 1, "npm run build", 11);

        let (_, agents) = list(&mut app);
        let pane_id = app.state.workspaces[1].tabs[0].root_pane;
        let pane_public = app.public_pane_id(1, pane_id);
        assert_eq!(agents.len(), 1);
        let attention = agents[0].attention.as_ref().expect("stuck");
        assert_eq!(attention.kind, AttentionKind::Stuck);
        assert_eq!(
            attention.reason,
            "`npm run build` has been running for over 10 min"
        );
        assert_eq!(attention.pane_id, pane_public);

        report(&mut app, 1, PaneAgentState::Idle, None);
        assert_eq!(list(&mut app).1[0].attention, None);

        let pane_events: Vec<_> = attention_events(&hub)
            .into_iter()
            .filter(|(item_id, _, _)| item_id.is_none())
            .collect();
        assert_eq!(
            pane_events,
            vec![
                (None, pane_public.clone(), Some(AttentionKind::Stuck)),
                (None, pane_public, None),
            ]
        );
    }

    #[test]
    fn a_command_that_has_not_run_long_enough_waits_for_the_stuck_period_to_end() {
        let (mut app, _hub) = app_with_item();
        run_command_for_minutes(&mut app, 1, "npm run build", 5);
        assert_eq!(list(&mut app).1[0].attention, None);

        // Attention is worked out again when the command will have run for ten minutes.
        let wait = app
            .attention_deadline()
            .expect("a recheck")
            .saturating_duration_since(Instant::now());
        assert!(
            (Duration::from_secs(290)..=Duration::from_secs(300)).contains(&wait),
            "{wait:?}"
        );
    }

    #[test]
    fn dismissing_a_stuck_agent_changes_nothing_because_its_command_has_to_end() {
        let (mut app, _hub) = app_with_item();
        run_command_for_minutes(&mut app, 1, "npm run build", 11);

        assert!(is_ok(&dismiss(&mut app, 1)));
        let attention = list(&mut app).1[0].attention.clone().expect("still stuck");
        assert_eq!(attention.kind, AttentionKind::Stuck);
    }

    #[test]
    fn a_working_agent_whose_transcript_shows_an_old_command_running_is_stuck() {
        use crate::agent_resume::{AgentSessionRef, PersistedAgentSession};

        let (mut app, _hub) = app_with_item();
        let started = SystemTime::now() - Duration::from_secs(11 * 60);
        let path = test_support::claude_transcript(
            "attention",
            &[("toolu_1", "cargo test --all", Some(started))],
        );
        report(&mut app, 1, PaneAgentState::Working, None);
        let terminal_id = terminal_of(&app, 1);
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.set_persisted_agent_session(PersistedAgentSession {
            source: "test".into(),
            agent: "claude".into(),
            session_ref: AgentSessionRef::path(path.to_str().expect("utf-8 path")).expect("path"),
        });
        terminal.working_since = Instant::now().checked_sub(Duration::from_secs(12 * 60));

        // The first look arms the schedule, and a minute later the transcript is read.
        let now = Instant::now();
        app.run_running_command_check(now);
        app.run_running_command_check(now + Duration::from_secs(60));
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.running_commands.is_empty() {
            assert!(Instant::now() < deadline, "the transcript was never read");
            app.drain_all_internal_events();
            std::thread::sleep(Duration::from_millis(10));
        }
        app.sync_work_item_events();

        let attention = list(&mut app).1[0].attention.clone().expect("stuck");
        assert_eq!(attention.kind, AttentionKind::Stuck);
        assert_eq!(
            attention.reason,
            "`cargo test --all` has been running for over 10 min"
        );
    }
}
