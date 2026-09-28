//! App-thread driver for attention: gathers each agent pane's signals, has work items
//! combine them with the items' own state, and publishes `attention.changed`.
//!
//! Worked out only when something it depends on changed (an agent's status, a pane or
//! workspace closing, the items themselves) or a finished turn's quiet period ends, never
//! per render or per byte of output.

use std::collections::HashMap;
use std::time::Instant;

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
        let item_workspaces = self.work_items.item_workspace_ids();
        let mut by_workspace = HashMap::new();
        let mut loose = Vec::new();
        let mut recheck_at = None;
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            let linked = item_workspaces.contains(ws.id.as_str());
            let mut verdicts = Vec::new();
            for signal in self.agent_signals(ws_idx) {
                let verdict = attention::agent_verdict(&signal, now);
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

    /// Every pane of workspace `ws_idx` running a detected agent.
    fn agent_signals(&self, ws_idx: usize) -> Vec<AgentSignal<'_>> {
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Vec::new();
        };
        let mut signals = Vec::new();
        for tab in &ws.tabs {
            for (pane_id, pane) in &tab.panes {
                let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id) else {
                    continue;
                };
                let Some(agent) = terminal.effective_agent_label() else {
                    continue;
                };
                let Some(public_pane_id) = self.public_pane_id(ws_idx, *pane_id) else {
                    continue;
                };
                signals.push(AgentSignal {
                    pane_id: public_pane_id,
                    agent,
                    state: terminal.state,
                    blocked_message: terminal
                        .hook_authority
                        .as_ref()
                        .filter(|authority| authority.state == crate::detect::AgentState::Blocked)
                        .and_then(|authority| authority.message.as_deref()),
                    turn_finished_at: terminal.turn_finished_at,
                    last_input_at: self
                        .terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .and_then(|runtime| runtime.last_user_input_at()),
                });
            }
        }
        signals
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
    use std::time::{Duration, Instant};

    use crate::api::schema::{
        AttentionKind, EmptyParams, EventData, Method, PaneAgentState, PaneReportAgentParams,
        Request, ResponseResult, SuccessResponse, WorkItemLinkParams,
    };
    use crate::app::{App, AppPolicy};
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
}
