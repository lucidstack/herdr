use super::*;
use crate::api::schema::{
    Method, WorkItemChoiceAction, WorkItemChoiceInfo, WorkItemInfo, WorkItemPhase,
    WorkItemProvisioningInfo, WorkItemSourceInfo, WorkItemStep, WorkItemStepInfo,
    WorkItemStepStatus,
};
use crate::protocol::work_items::EndpointWorkItemsProjection;

fn item(id: &str) -> WorkItemInfo {
    WorkItemInfo {
        item_id: format!("github:o/r#{id}"),
        source_id: "github".into(),
        context: format!("o/r #{id}"),
        title: format!("Pull request {id}"),
        author: Some("alice".into()),
        url: format!("https://github.com/o/r/pull/{id}"),
        summary: Some("+3 −1 across 1 file".into()),
        notice: None,
        phase: WorkItemPhase::Pending,
        seen: false,
        resolved: false,
        workspace_id: None,
        choices: vec![
            WorkItemChoiceInfo {
                choice_id: "local".into(),
                label: "Review locally".into(),
                description: None,
                action: WorkItemChoiceAction::ProvisionWorkspace,
                disabled_reason: None,
            },
            WorkItemChoiceInfo {
                choice_id: "github".into(),
                label: "Review on GitHub".into(),
                description: None,
                action: WorkItemChoiceAction::OpenUrl {
                    url: format!("https://github.com/o/r/pull/{id}"),
                },
                disabled_reason: None,
            },
        ],
        default_choice_id: Some("github".into()),
        provisioning: None,
    }
}

fn projection(revision: u64, items: Vec<WorkItemInfo>) -> EndpointWorkItemsProjection {
    EndpointWorkItemsProjection {
        boot_id: "boot-1".into(),
        revision,
        sources: vec![WorkItemSourceInfo {
            source_id: "github".into(),
            label: "GitHub".into(),
            error: None,
        }],
        items,
    }
}

fn two_workspace_snapshot() -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    let mut second = snapshot.workspaces[0].clone();
    second.workspace_id = "ws_2".into();
    second.number = 2;
    second.label = "review-space".into();
    second.branch = None;
    second.focused = false;
    snapshot.workspaces.push(second);
    snapshot
}

fn shell_with(items: Vec<WorkItemInfo>) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(two_workspace_snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(1, items));
    state
}

fn screen_text(state: &mut ClientShellState) -> String {
    frame_rows(&state.compose(106, 30).expect("frame")).join("\n")
}

fn click_item(state: &mut ClientShellState, index: usize) -> ClientShellInput {
    let rect = state.hits.work_items[index].rect;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + 2,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    })])
}

fn endpoint_methods(input: &ClientShellInput) -> Vec<&Method> {
    input
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(&request.method),
            _ => None,
        })
        .collect()
}

#[test]
fn sidebar_has_no_inbox_without_a_projection() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(two_workspace_snapshot()));
    assert!(!screen_text(&mut state).contains("inbox"));
}

#[test]
fn unseen_item_renders_in_the_inbox() {
    let mut state = shell_with(vec![item("7")]);
    let text = screen_text(&mut state);
    assert!(text.contains("inbox"), "{text}");
    assert!(text.contains("1 new"), "{text}");
    assert!(text.contains("o/r #7"), "{text}");
    assert!(text.contains("Pull request 7"), "{text}");
}

#[test]
fn owned_workspace_nests_under_its_item_only() {
    let mut owned = item("7");
    owned.workspace_id = Some("ws_2".into());
    owned.phase = WorkItemPhase::Local;
    let mut state = shell_with(vec![owned]);
    let text = screen_text(&mut state);
    assert_eq!(text.matches("review-space").count(), 1, "{text}");
    let item_row = text
        .lines()
        .position(|line| line.contains("o/r #7"))
        .expect("item row");
    let workspace_row = text
        .lines()
        .position(|line| line.contains("review-space"))
        .expect("workspace row");
    let spaces_row = text
        .lines()
        .position(|line| line.contains(" spaces"))
        .expect("spaces header");
    assert!(
        item_row < workspace_row && workspace_row < spaces_row,
        "{text}"
    );
}

#[test]
fn workspace_of_an_overflowed_item_stays_in_the_spaces_list() {
    let mut items: Vec<_> = (1..=10).map(|n| item(&n.to_string())).collect();
    let owner = items.last_mut().expect("items");
    owner.workspace_id = Some("ws_2".into());
    owner.phase = WorkItemPhase::Local;
    let mut state = shell_with(items);
    let text = screen_text(&mut state);
    assert!(text.contains("more"), "{text}");
    assert!(!text.contains("o/r #10"), "{text}");
    assert_eq!(text.matches("review-space").count(), 1, "{text}");
}

#[test]
fn clicking_unseen_item_marks_it_seen_and_opens_dialog_on_default_choice() {
    let mut state = shell_with(vec![item("7")]);
    state.compose(106, 30).expect("frame");
    let input = click_item(&mut state, 0);
    assert!(matches!(
        endpoint_methods(&input)[..],
        [Method::WorkItemMarkSeen(target)] if target.item_id == "github:o/r#7"
    ));
    let Some(ClientShellOverlay::WorkItem(overlay)) = state.overlay.as_ref() else {
        panic!("work item dialog should open");
    };
    assert_eq!(
        overlay.item.choices[overlay.highlighted].choice_id,
        "github"
    );
}

#[test]
fn dialog_lists_choices_and_arrow_keys_skip_disabled_ones() {
    let mut blocked = item("7");
    blocked.seen = true;
    blocked.choices.insert(
        1,
        WorkItemChoiceInfo {
            choice_id: "agent_post".into(),
            label: "Agent review, post on GitHub".into(),
            description: None,
            action: WorkItemChoiceAction::ProvisionWorkspace,
            disabled_reason: Some("No agent configured".into()),
        },
    );
    blocked.default_choice_id = Some("local".into());
    let mut state = shell_with(vec![blocked]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    let text = screen_text(&mut state);
    assert!(text.contains("Review locally"), "{text}");
    assert!(text.contains("Agent review, post on GitHub"), "{text}");
    assert!(text.contains("Review on GitHub"), "{text}");
    state.handle_input_bytes(b"\x1b[B");
    let Some(ClientShellOverlay::WorkItem(overlay)) = state.overlay.as_ref() else {
        panic!("dialog stays open");
    };
    assert_eq!(
        overlay.item.choices[overlay.highlighted].choice_id,
        "github"
    );
}

#[test]
fn confirming_external_choice_opens_url_and_reports_the_choice() {
    let mut seen = item("7");
    seen.seen = true;
    let mut state = shell_with(vec![seen]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    let input = state.handle_input_bytes(b"\r");
    assert!(input.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::OpenSafeWebUrl(url) if url == "https://github.com/o/r/pull/7"
    )));
    assert!(matches!(
        endpoint_methods(&input)[..],
        [Method::WorkItemChoose(params)]
            if params.item_id == "github:o/r#7" && params.choice_id == "github"
    ));
    assert!(state.overlay.is_none());
}

#[test]
fn dialog_closes_when_its_item_disappears() {
    let mut state = shell_with(vec![item("7")]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    assert!(state.overlay.is_some());
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(2, Vec::new()));
    assert!(state.overlay.is_none());
}

#[test]
fn spinner_ticks_only_while_an_item_awaits_the_source() {
    let mut state = shell_with(vec![item("7")]);
    let now = std::time::Instant::now();
    assert!(!state.tick_work_items(now));
    let mut awaiting = item("7");
    awaiting.phase = WorkItemPhase::AwaitingExternal;
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(2, vec![awaiting]));
    assert!(state.tick_work_items(now));
}

fn provisioning_item(workspace_id: Option<&str>) -> WorkItemInfo {
    let mut provisioning = item("7");
    provisioning.seen = true;
    provisioning.phase = WorkItemPhase::Local;
    provisioning.workspace_id = workspace_id.map(str::to_owned);
    provisioning.provisioning = Some(WorkItemProvisioningInfo {
        steps: vec![WorkItemStepInfo {
            step: WorkItemStep::Checkout,
            label: "Branch checked out".into(),
            status: WorkItemStepStatus::Running,
            detail: None,
        }],
        finished: false,
    });
    provisioning
}

#[test]
fn checklist_escape_returns_to_the_previous_workspace() {
    let mut state = shell_with(vec![provisioning_item(Some("ws_2"))]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::WorkItem(overlay)) if overlay.checklist()
    ));
    let mut moved = two_workspace_snapshot();
    moved.revision = 2;
    moved.focused_workspace_id = Some("ws_2".into());
    state.set_snapshot(Box::new(moved));
    let input = state.handle_input_bytes(b"\x1b");
    assert!(matches!(
        endpoint_methods(&input)[..],
        [Method::WorkspaceFocus(target)] if target.workspace_id == "ws_1"
    ));
    assert!(state.overlay.is_none());
}

#[test]
fn checklist_enter_focuses_the_provisioned_workspace() {
    let mut state = shell_with(vec![provisioning_item(Some("ws_2"))]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    let input = state.handle_input_bytes(b"\r");
    assert!(matches!(
        endpoint_methods(&input)[..],
        [Method::WorkspaceFocus(target)] if target.workspace_id == "ws_2"
    ));
}
