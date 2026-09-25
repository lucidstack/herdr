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
        dismissed: false,
        snoozed_until: None,
        choices: vec![
            WorkItemChoiceInfo {
                choice_id: "local".into(),
                label: "Review locally".into(),
                description: None,
                action: WorkItemChoiceAction::ProvisionWorkspace,
                disabled_reason: None,
                confirm: None,
            },
            WorkItemChoiceInfo {
                choice_id: "github".into(),
                label: "Review on GitHub".into(),
                description: None,
                action: WorkItemChoiceAction::OpenUrl {
                    url: format!("https://github.com/o/r/pull/{id}"),
                },
                disabled_reason: None,
                confirm: None,
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
            label: "Ask agent to review and comment on GitHub".into(),
            description: None,
            action: WorkItemChoiceAction::ProvisionWorkspace,
            disabled_reason: Some("No agent configured".into()),
            confirm: None,
        },
    );
    blocked.default_choice_id = Some("local".into());
    let mut state = shell_with(vec![blocked]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    let text = screen_text(&mut state);
    assert!(text.contains("Review locally"), "{text}");
    assert!(
        text.contains("Ask agent to review and comment on GitHub"),
        "{text}"
    );
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

fn mouse(
    state: &mut ClientShellState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })])
}

#[test]
fn hidden_items_leave_the_sidebar_and_are_counted() {
    let mut dismissed = item("7");
    dismissed.dismissed = true;
    let mut snoozed = item("8");
    snoozed.snoozed_until = Some(4_000_000_000);
    let mut state = shell_with(vec![dismissed, snoozed, item("9")]);
    let text = screen_text(&mut state);
    assert!(
        !text.contains("o/r #7") && !text.contains("o/r #8"),
        "{text}"
    );
    assert!(text.contains("o/r #9"), "{text}");
    let mut seen = item("9");
    seen.seen = true;
    let mut dismissed = item("7");
    dismissed.dismissed = true;
    state.set_endpoint_work_items(
        &ClientEndpointId::Local,
        projection(2, vec![dismissed, seen]),
    );
    assert!(screen_text(&mut state).contains("1 hidden"));
}

#[test]
fn overflowed_items_are_reachable_by_expanding_and_scrolling() {
    let items: Vec<_> = (1..=10).map(|n| item(&n.to_string())).collect();
    let mut state = shell_with(items);
    state.compose(106, 30).expect("frame");
    let shown_collapsed = state.hits.inbox.shown;
    let more = state.hits.inbox.more_below;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        more.x + 1,
        more.y,
    );
    state.compose(106, 30).expect("frame");
    assert!(state.hits.inbox.shown > shown_collapsed);
    assert!(!screen_text(&mut state).contains("o/r #10"));

    let area = state.hits.inbox.area;
    for _ in 0..9 {
        mouse(
            &mut state,
            MouseEventKind::ScrollDown,
            area.x + 2,
            area.y + 2,
        );
    }
    let text = screen_text(&mut state);
    assert!(text.contains("o/r #10"), "{text}");
    assert!(text.contains("↑ 9 more"), "{text}");
}

#[test]
fn item_context_menu_dismisses_the_item() {
    let mut state = shell_with(vec![item("7")]);
    state.compose(106, 30).expect("frame");
    let rect = state.hits.work_items[0].rect;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Right),
        rect.x + 2,
        rect.y,
    );
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("item menu opens");
    };
    let dismiss = menu
        .items()
        .iter()
        .position(|entry| entry.label == "Dismiss")
        .expect("dismiss offered");
    state.compose(106, 30).expect("frame");
    let (row, _) = state.hits.context_menu_rows[dismiss];
    let input = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        row.x + 1,
        row.y,
    );
    assert!(matches!(
        endpoint_methods(&input)[..],
        [Method::WorkItemHide(params)] if params.item_id == "github:o/r#7" && params.snooze_seconds.is_none()
    ));
}

#[test]
fn inbox_keybinding_lists_items_for_the_keyboard() {
    let mut dismissed = item("8");
    dismissed.dismissed = true;
    let mut state = shell_with(vec![dismissed, item("7")]);
    let mut open = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::OpenInbox),
        &mut open,
    );
    let Some(ClientShellOverlay::Inbox(inbox)) = state.overlay.as_ref() else {
        panic!("inbox opens");
    };
    let order: Vec<&str> = inbox
        .items
        .iter()
        .map(|item| item.item_id.as_str())
        .collect();
    assert_eq!(order, vec!["github:o/r#7", "github:o/r#8"]);

    let snooze = state.handle_input_bytes(b"s");
    assert!(matches!(
        endpoint_methods(&snooze)[..],
        [Method::WorkItemHide(params)] if params.item_id == "github:o/r#7" && params.snooze_seconds == Some(3600)
    ));
    state.handle_input_bytes(b"j");
    let unhide = state.handle_input_bytes(b"u");
    assert!(matches!(
        endpoint_methods(&unhide)[..],
        [Method::WorkItemUnhide(target)] if target.item_id == "github:o/r#8"
    ));
    state.handle_input_bytes(b"k");
    state.handle_input_bytes(b"\r");
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::WorkItem(overlay)) if overlay.item.item_id == "github:o/r#7"
    ));
}

#[test]
fn collapsed_sidebar_badge_counts_new_items_and_opens_the_inbox() {
    let mut seen = item("8");
    seen.seen = true;
    let mut state = shell_with(vec![item("7"), seen]);
    state.sidebar_collapsed = true;
    state.compose(106, 30).expect("frame");
    let badge = state.hits.inbox.badge;
    assert!(!badge.is_empty());
    assert!(screen_text(&mut state)
        .lines()
        .any(|line| line.starts_with("●1")));
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        badge.x,
        badge.y,
    );
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::Inbox(inbox)) if inbox.items.len() == 2
    ));
}

#[test]
fn item_menu_links_the_focused_workspace() {
    let mut state = shell_with(vec![item("7")]);
    state.compose(106, 30).expect("frame");
    let rect = state.hits.work_items[0].rect;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Right),
        rect.x + 2,
        rect.y,
    );
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("item menu opens");
    };
    let link = menu
        .items()
        .iter()
        .position(|entry| entry.label == "Link to current workspace")
        .expect("link offered");
    state.compose(106, 30).expect("frame");
    let (row, _) = state.hits.context_menu_rows[link];
    let input = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        row.x + 1,
        row.y,
    );
    assert!(matches!(
        endpoint_methods(&input)[..],
        [Method::WorkItemLink(params)]
            if params.item_id == "github:o/r#7" && params.workspace_id == "ws_1"
    ));
}

#[test]
fn irreversible_choice_runs_only_after_a_second_confirm() {
    let mut ready = item("7");
    ready.seen = true;
    ready.choices.insert(
        0,
        WorkItemChoiceInfo {
            choice_id: "merge_squash".into(),
            label: "Squash and merge".into(),
            description: None,
            action: WorkItemChoiceAction::Perform,
            disabled_reason: None,
            confirm: Some("Merge #7 into main? This cannot be undone.".into()),
        },
    );
    ready.default_choice_id = Some("merge_squash".into());
    let mut state = shell_with(vec![ready]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);

    let first = state.handle_input_bytes(b"\r");
    assert!(endpoint_methods(&first).is_empty());
    assert!(screen_text(&mut state).contains("This cannot be undone"));
    // Moving away cancels the pending confirmation.
    state.handle_input_bytes(b"j");
    state.handle_input_bytes(b"k");
    assert!(endpoint_methods(&state.handle_input_bytes(b"\r")).is_empty());

    let second = state.handle_input_bytes(b"\r");
    assert!(matches!(
        endpoint_methods(&second)[..],
        [Method::WorkItemChoose(params)] if params.choice_id == "merge_squash"
    ));
    assert!(state.overlay.is_none());
}
