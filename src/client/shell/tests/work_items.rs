use super::*;
use crate::api::schema::{
    Method, WorkItemChoiceAction, WorkItemChoiceInfo, WorkItemChoiceOptionInfo, WorkItemInfo,
    WorkItemPhase, WorkItemProvisioningInfo, WorkItemSourceInfo, WorkItemStep, WorkItemStepInfo,
    WorkItemStepStatus,
};
use crate::protocol::work_items::EndpointWorkItemsProjection;

pub(super) fn item(id: &str) -> WorkItemInfo {
    WorkItemInfo {
        item_id: format!("github:o/r#{id}"),
        source_id: "github".into(),
        context: format!("o/r #{id}"),
        tracker_state: None,
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
                choice_id: "review".into(),
                label: "Review".into(),
                description: None,
                action: WorkItemChoiceAction::ProvisionWorkspace,
                disabled_reason: None,
                confirm: None,
                options: Vec::new(),
            },
            WorkItemChoiceInfo {
                choice_id: "open".into(),
                label: "Open in the browser".into(),
                description: None,
                action: WorkItemChoiceAction::OpenUrl {
                    url: format!("https://github.com/o/r/pull/{id}"),
                },
                disabled_reason: None,
                confirm: None,
                options: Vec::new(),
            },
        ],
        default_choice_id: Some("open".into()),
        provisioning: None,
        is_pick_next: false,
        start_reminder: None,
        running_choice_id: None,
        action_outcome: None,
        linked_pull_request: None,
        own_pull_request: None,
        linked_ticket: None,
        folded_into: None,
        attention: None,
    }
}

pub(super) fn projection(revision: u64, items: Vec<WorkItemInfo>) -> EndpointWorkItemsProjection {
    EndpointWorkItemsProjection {
        boot_id: "boot-1".into(),
        revision,
        sources: vec![WorkItemSourceInfo {
            source_id: "github".into(),
            label: "GitHub".into(),
            error: None,
        }],
        items,
        pick_next: Default::default(),
        repositories: Vec::new(),
    }
}

pub(super) fn two_workspace_snapshot() -> ClientShellSnapshot {
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
fn next_workspace_follows_the_inbox_order_before_the_spaces_list() {
    // Spaces list order is ws_1, ws_2, ws_3; the inbox lists ws_3's ticket above ws_2's.
    let mut snapshot = two_workspace_snapshot();
    let mut third = snapshot.workspaces[1].clone();
    third.workspace_id = "ws_3".into();
    third.number = 3;
    third.label = "third-space".into();
    snapshot.workspaces.push(third);
    let mut first_ticket = item("7");
    first_ticket.workspace_id = Some("ws_3".into());
    let mut second_ticket = item("8");
    second_ticket.workspace_id = Some("ws_2".into());
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    state.set_endpoint_work_items(
        &ClientEndpointId::Local,
        projection(1, vec![first_ticket, second_ticket]),
    );

    // ws_1 is focused and sits last, below the inbox: next wraps to the top ticket.
    let mut next = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextWorkspace),
        &mut next,
    );
    assert!(
        matches!(
            endpoint_methods(&next)[..],
            [Method::WorkspaceFocus(target)] if target.workspace_id == "ws_3"
        ),
        "{:?}",
        endpoint_methods(&next)
    );
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
fn item_stands_for_its_workspace_with_the_agent_status_on_its_first_row() {
    let mut owned = item("7");
    owned.workspace_id = Some("ws_2".into());
    owned.phase = WorkItemPhase::Local;
    let mut snapshot = two_workspace_snapshot();
    snapshot.workspaces[1].agent_status = crate::api::schema::AgentStatus::Blocked;
    let config = ClientShellConfig::from_config(&Config::default());
    let expected = (
        crate::client::shell::status_icon(
            crate::api::schema::AgentStatus::Blocked,
            config.status_indicators,
        ),
        crate::client::shell::status_color(
            crate::api::schema::AgentStatus::Blocked,
            &config.palette,
        ),
    );
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(1, vec![owned]));

    let frame = state.compose(106, 30).expect("frame");
    let text = frame_rows(&frame).join("\n");
    // Neither nested under the item nor in the spaces list.
    assert!(!text.contains("review-space"), "{text}");
    let rect = state.hits.work_items[0].rect;
    let buffer = frame.to_ratatui_buffer().expect("buffer");
    let cell = buffer
        .cell((rect.right().saturating_sub(2), rect.y))
        .expect("status cell");
    assert_eq!((cell.symbol(), cell.fg), expected, "{text}");
}

#[test]
fn pull_request_item_shows_its_own_state_on_a_line_of_its_own() {
    let mut pull = item("8");
    pull.own_pull_request = Some(crate::api::schema::WorkItemPullRequestInfo {
        source_id: "github".into(),
        repo: "o/r".into(),
        number: 8,
        url: "https://github.com/o/r/pull/8".into(),
        is_draft: false,
        status: "approved · CI failing".into(),
    });
    let mut state = shell_with(vec![pull, item("9")]);
    state.compose(106, 30).expect("frame");
    assert_eq!(state.hits.work_items[0].rect.height, 3);
    let text = screen_text(&mut state);
    let lines: Vec<&str> = text.lines().collect();
    let title_row = lines
        .iter()
        .position(|line| line.contains("Pull request 8"))
        .expect("title drawn");
    // The sidebar truncates the rest of the line.
    assert!(lines[title_row + 1].contains("gh approved · CI"), "{text}");
}

#[test]
fn pull_request_item_dialog_heading_says_where_it_stands() {
    let mut pull = item("8");
    pull.own_pull_request = Some(crate::api::schema::WorkItemPullRequestInfo {
        source_id: "github".into(),
        repo: "o/r".into(),
        number: 8,
        url: "https://github.com/o/r/pull/8".into(),
        is_draft: true,
        status: "draft".into(),
    });
    let mut state = shell_with(vec![pull]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::WorkItem(_))
    ));
    let text = screen_text(&mut state);
    assert!(text.contains("o/r #8 · draft · @alice"), "{text}");
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
    assert_eq!(overlay.item.choices[overlay.highlighted].choice_id, "open");
}

#[test]
fn dialog_lists_choices_and_arrow_keys_skip_disabled_ones() {
    let mut blocked = item("7");
    blocked.seen = true;
    blocked.choices.insert(
        1,
        WorkItemChoiceInfo {
            choice_id: "push_reply".into(),
            label: "Ask agent to push and reply".into(),
            description: None,
            action: WorkItemChoiceAction::ProvisionWorkspace,
            disabled_reason: Some("Work on it locally first".into()),
            confirm: None,
            options: Vec::new(),
        },
    );
    blocked.default_choice_id = Some("review".into());
    let mut state = shell_with(vec![blocked]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    let text = screen_text(&mut state);
    assert!(text.contains("Review"), "{text}");
    assert!(text.contains("Ask agent to push and reply"), "{text}");
    assert!(text.contains("Open in the browser"), "{text}");
    state.handle_input_bytes(b"\x1b[B");
    let Some(ClientShellOverlay::WorkItem(overlay)) = state.overlay.as_ref() else {
        panic!("dialog stays open");
    };
    assert_eq!(overlay.item.choices[overlay.highlighted].choice_id, "open");
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
            if params.item_id == "github:o/r#7" && params.choice_id == "open"
    ));
    assert!(state.overlay.is_none());
}

fn remote_shell_with(items: Vec<WorkItemInfo>) -> ClientShellState {
    let mut state = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_remote_viewer(true),
    );
    state.set_snapshot(Box::new(two_workspace_snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(1, items));
    state
}

fn opens_locally(input: &ClientShellInput) -> bool {
    input
        .actions
        .iter()
        .any(|action| matches!(action, ClientShellAction::OpenSafeWebUrl(_)))
}

fn copied(input: &ClientShellInput) -> Option<String> {
    input.actions.iter().find_map(|action| match action {
        ClientShellAction::ClipboardWrite(bytes) => String::from_utf8(bytes.clone()).ok(),
        _ => None,
    })
}

#[test]
fn remote_viewer_gets_the_link_to_copy_instead_of_a_browser_on_the_host() {
    let mut seen = item("7");
    seen.seen = true;
    let mut state = remote_shell_with(vec![seen]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);

    let input = state.handle_input_bytes(b"\r");
    assert!(!opens_locally(&input));
    // The source still hears about the choice, so the item waits for you to finish there.
    assert!(matches!(
        endpoint_methods(&input)[..],
        [Method::WorkItemChoose(params)] if params.choice_id == "open"
    ));
    let screen = screen_text(&mut state);
    assert!(screen.contains("https://github.com/o/r/pull/7"));

    let copy = state.handle_input_bytes(b"\r");
    assert_eq!(
        copied(&copy).as_deref(),
        Some("https://github.com/o/r/pull/7")
    );
    assert!(state.overlay.is_none());
}

#[test]
fn tapping_the_shown_link_copies_it() {
    let mut state = remote_shell_with(vec![item("7")]);
    state.compose(106, 30).expect("frame");
    let mut outcome = ClientShellInput::default();
    state.open_web_link("https://github.com/o/r/pull/7".into(), &mut outcome);
    state.compose(106, 30).expect("frame");
    let link = state.hits.overlay_clear;
    let tap = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        link.x + 3,
        link.y,
    );
    assert_eq!(
        copied(&tap).as_deref(),
        Some("https://github.com/o/r/pull/7")
    );
}

#[test]
fn open_links_setting_overrides_where_the_client_runs() {
    let url = || "https://github.com/o/r/pull/7".to_string();
    let mut local = Config::default();
    local.ui.open_links = crate::config::OpenLinksConfig::Local;
    let mut remote_but_local =
        ClientShellState::new(ClientShellConfig::from_config(&local).with_remote_viewer(true));
    let mut outcome = ClientShellInput::default();
    remote_but_local.open_web_link(url(), &mut outcome);
    assert!(opens_locally(&outcome));

    let mut show = Config::default();
    show.ui.open_links = crate::config::OpenLinksConfig::Show;
    let mut here_but_shown = ClientShellState::new(ClientShellConfig::from_config(&show));
    let mut outcome = ClientShellInput::default();
    here_but_shown.open_web_link(url(), &mut outcome);
    assert!(!opens_locally(&outcome));
    assert!(matches!(
        here_but_shown.overlay,
        Some(ClientShellOverlay::Link(_))
    ));
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

#[test]
fn emptying_the_inbox_sparkles_briefly_then_settles_on_all_clear() {
    // An inbox that starts out empty just says so.
    let mut state = shell_with(Vec::new());
    let text = screen_text(&mut state);
    assert!(text.contains("all clear"), "{text}");
    assert!(!text.contains("inbox zero"), "{text}");

    let mut state = shell_with(vec![item("7")]);
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(2, Vec::new()));
    let text = screen_text(&mut state);
    assert!(text.contains("inbox zero"), "{text}");

    // The sparkle animates on the spinner clock and ends on its own.
    let now = std::time::Instant::now();
    assert!(state.tick_work_items(now));
    assert!(state.tick_work_items(now + std::time::Duration::from_secs(5)));
    let text = screen_text(&mut state);
    assert!(text.contains("all clear"), "{text}");
    assert!(!state.tick_work_items(now + std::time::Duration::from_secs(6)));
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
        finished_at: None,
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

/// A finished provisioning whose agent brief failed, as the server projects it. `attention` is
/// what the item needs you for: the failure, until an agent took a turn after it.
fn brief_failed_item(attention: Option<crate::api::schema::AttentionKind>) -> WorkItemInfo {
    let step = |step, label: &str, status, detail: Option<&str>| WorkItemStepInfo {
        step,
        label: label.into(),
        status,
        detail: detail.map(str::to_owned),
    };
    let mut failed = provisioning_item(Some("ws_2"));
    failed.provisioning = Some(WorkItemProvisioningInfo {
        steps: vec![
            step(
                WorkItemStep::Checkout,
                "Worktree created",
                WorkItemStepStatus::Done,
                None,
            ),
            step(
                WorkItemStep::AgentBrief,
                "Agent briefed",
                WorkItemStepStatus::Failed,
                Some("agent still idle 15 s after the brief"),
            ),
        ],
        finished: true,
        finished_at: Some(1_000),
    });
    failed.attention = attention.map(|kind| crate::api::schema::AttentionInfo {
        kind,
        reason: "agent still idle 15 s after the brief".into(),
        pane_id: None,
        since: 1_000,
    });
    failed
}

/// The line of the screen that names item 7, its sidebar row.
fn row_of_item_7(state: &mut ClientShellState) -> String {
    let text = screen_text(state);
    text.lines()
        .find(|line| line.contains("o/r #7"))
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("the item is listed: {text}"))
}

#[test]
fn a_failed_provisioning_marks_its_row_only_while_the_failure_needs_you() {
    use crate::api::schema::AttentionKind;

    let mut state = shell_with(vec![brief_failed_item(Some(AttentionKind::Failed))]);
    let row = row_of_item_7(&mut state);
    assert!(row.contains('✗'), "{row}");

    // An agent took a turn after the failure: the step stays failed, but nothing needs you
    // now, so the row shows the agent's status like that of any item that needs nothing.
    state.set_endpoint_work_items(
        &ClientEndpointId::Local,
        projection(2, vec![brief_failed_item(None)]),
    );
    let row = row_of_item_7(&mut state);
    assert!(!row.contains('✗'), "{row}");
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
fn pull_request_shows_the_ticket_its_title_names_on_a_line_of_its_own() {
    let mut pull = item("8");
    pull.title = "[TECH-7] Fix login".into();
    pull.linked_ticket = Some(crate::api::schema::WorkItemLinkedTicketInfo {
        source_id: "jira".into(),
        key: "TECH-7".into(),
        url: "https://example.atlassian.net/browse/TECH-7".into(),
        tracker_state: "In Progress · Ada".into(),
    });
    let mut state = shell_with(vec![pull, item("9")]);
    state.compose(106, 30).expect("frame");
    assert_eq!(state.hits.work_items[0].rect.height, 3);
    let text = screen_text(&mut state);
    let lines: Vec<&str> = text.lines().collect();
    let title_row = lines
        .iter()
        .position(|line| line.contains("[TECH-7] Fix login"))
        .expect("title drawn");
    // The sidebar truncates the rest of the line.
    assert!(lines[title_row + 1].contains("jira TECH-7 · In"), "{text}");
}

#[test]
fn ticket_shows_a_status_line_per_service_and_its_pull_request_folds_in() {
    use crate::api::schema::WorkItemPullRequestInfo;

    let mut ticket = item("7");
    ticket.item_id = "jira:TECH-7".into();
    ticket.source_id = "jira".into();
    ticket.tracker_state = Some("In Progress · Ada".into());
    ticket.linked_pull_request = Some(WorkItemPullRequestInfo {
        source_id: "github".into(),
        repo: "o/r".into(),
        number: 8,
        url: "https://github.com/o/r/pull/8".into(),
        is_draft: true,
        status: "draft".into(),
    });
    // The pull request's own inbox item shows with the ticket, not on its own.
    let mut pull = item("8");
    pull.folded_into = Some("jira:TECH-7".into());
    let mut state = shell_with(vec![ticket, pull, item("9")]);
    state.compose(106, 30).expect("frame");
    assert_eq!(
        state.hits.inbox.visible, 2,
        "the folded pull request is not listed on its own"
    );
    let first = &state.hits.work_items[0];
    assert_eq!(first.item_id, "jira:TECH-7");
    assert_eq!(first.rect.height, 4);
    let rect = first.rect;
    let text = screen_text(&mut state);
    let lines: Vec<&str> = text.lines().collect();
    let title_row = lines
        .iter()
        .position(|line| line.contains("Pull request 7"))
        .expect("title drawn");
    assert!(lines[title_row + 1].contains("jira In Progress"), "{text}");
    assert!(lines[title_row + 2].contains("gh #8 · draft"), "{text}");

    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Right),
        rect.x + 2,
        rect.y,
    );
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("item menu opens");
    };
    let labels: Vec<String> = menu
        .items()
        .iter()
        .map(|entry| entry.label.to_string())
        .collect();
    // The pull request's choices sit under its own header, below the ticket's.
    let header = labels
        .iter()
        .position(|label| label == "o/r #8")
        .expect("pull request header");
    assert_eq!(labels[header + 1], "Review", "{labels:?}");
    assert!(
        labels.contains(&"Open #8 on GitHub".to_string()),
        "{labels:?}"
    );
    state.compose(106, 30).expect("frame");
    let (row, _) = state.hits.context_menu_rows[header + 1];
    let chosen = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        row.x + 1,
        row.y,
    );
    assert!(
        endpoint_methods(&chosen).iter().any(|method| matches!(
            method,
            Method::WorkItemChoose(params)
                if params.item_id == "github:o/r#8" && params.choice_id == "review"
        )),
        "the pull request's choice runs, not the ticket's"
    );
}

#[test]
fn item_menu_leads_with_a_started_items_tracker_fix() {
    let mut started = item("7");
    started.seen = true;
    started.workspace_id = Some("ws_2".into());
    started.choices.insert(
        0,
        WorkItemChoiceInfo {
            choice_id: "start_work".into(),
            label: "Assign to me".into(),
            description: None,
            action: WorkItemChoiceAction::Perform,
            disabled_reason: None,
            confirm: None,
            options: Vec::new(),
        },
    );
    started.default_choice_id = Some("start_work".into());
    let mut state = shell_with(vec![started]);
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
    let items = menu.items();
    assert_eq!(items[menu.highlighted].label, "Assign to me");
    // A left click already opens the workspace, so the menu does not offer it again.
    assert!(items.iter().all(|entry| entry.label != "Review"));
    state.compose(106, 30).expect("frame");
    let (row, _) = state.hits.context_menu_rows[menu_index(&state, "Assign to me")];
    let chosen = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        row.x + 1,
        row.y,
    );
    assert!(endpoint_methods(&chosen).iter().any(|method| matches!(
        method,
        Method::WorkItemChoose(params) if params.choice_id == "start_work"
    )));
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::WorkItem(overlay))
            if overlay.item.running_choice_id.as_deref() == Some("start_work")
    ));
}

fn menu_index(state: &ClientShellState, label: &str) -> usize {
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("menu open");
    };
    menu.items()
        .iter()
        .position(|entry| entry.label == label)
        .unwrap_or_else(|| panic!("{label} offered"))
}

#[test]
fn start_reminder_highlights_the_tracker_line_not_the_item() {
    let mut started = item("7");
    started.seen = true;
    started.workspace_id = Some("ws_2".into());
    started.tracker_state = Some("To Do · unassigned".into());
    started.start_reminder = Some("You're working on this, but it isn't assigned to you".into());
    let mut state = shell_with(vec![started]);
    let frame = state.compose(106, 30).expect("frame");
    let buffer = frame.to_ratatui_buffer().expect("buffer");
    let rect = state.hits.work_items[0].rect;
    let palette = &state.config.palette;

    let tracker = cell_symbol_position(&frame, rect, "To Do");
    assert_eq!(buffer[tracker].bg, palette.yellow);
    let context = cell_symbol_position(&frame, rect, "o/r #7");
    assert_eq!(buffer[context].fg, palette.mauve);
    assert_ne!(buffer[context].bg, palette.yellow);
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
            options: Vec::new(),
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
    assert!(
        matches!(
            state.overlay.as_ref(),
            Some(ClientShellOverlay::WorkItem(_))
        ),
        "the dialog stays to show the outcome"
    );
}

#[test]
fn performed_choice_shows_its_outcome_and_retries_only_after_failure() {
    use crate::api::schema::WorkItemActionOutcome;

    let mut started = item("7");
    started.seen = true;
    started.choices.insert(
        0,
        WorkItemChoiceInfo {
            choice_id: "start_work".into(),
            label: "Assign to me and move to In Progress".into(),
            description: None,
            action: WorkItemChoiceAction::Perform,
            disabled_reason: None,
            confirm: None,
            options: Vec::new(),
        },
    );
    started.default_choice_id = Some("start_work".into());
    let mut state = shell_with(vec![started.clone()]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    let sent = state.handle_input_bytes(b"\r");
    assert!(matches!(
        endpoint_methods(&sent)[..],
        [Method::WorkItemChoose(params)] if params.choice_id == "start_work"
    ));
    // Running: a second Enter sends nothing.
    assert!(endpoint_methods(&state.handle_input_bytes(b"\r")).is_empty());

    let refused = "Jira refused the change: the API token needs write:jira-work to assign and \
                   start issues. After replacing it, restart the Herdr server so it reads the new token";
    let mut failed = started.clone();
    failed.notice = Some(refused.into());
    failed.action_outcome = Some(WorkItemActionOutcome {
        choice_id: "start_work".into(),
        succeeded: false,
        message: refused.into(),
    });
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(2, vec![failed]));
    let text = screen_text(&mut state);
    assert!(
        text.contains("restart the Herdr server"),
        "the reason wraps: {text}"
    );
    assert!(matches!(
        endpoint_methods(&state.handle_input_bytes(b"\r"))[..],
        [Method::WorkItemChoose(params)] if params.choice_id == "start_work"
    ));

    let mut done = started;
    done.action_outcome = Some(WorkItemActionOutcome {
        choice_id: "start_work".into(),
        succeeded: true,
        message: "TECH-7 is yours and In Progress".into(),
    });
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(3, vec![done]));
    let text = screen_text(&mut state);
    assert!(text.contains("✓ TECH-7 is yours and In Progress"), "{text}");
    assert!(
        text.contains("Assign to me and move to In Progress ✓"),
        "{text}"
    );
    assert!(
        endpoint_methods(&state.handle_input_bytes(b"\r")).is_empty(),
        "a finished fix is not repeated"
    );
}

#[test]
fn briefing_the_agent_closes_the_dialog_and_asks_the_server() {
    let mut local = item("7");
    local.seen = true;
    local.choices.insert(
        0,
        WorkItemChoiceInfo {
            choice_id: "push_reply".into(),
            label: "Ask agent to push and reply".into(),
            description: None,
            action: WorkItemChoiceAction::BriefAgent,
            disabled_reason: None,
            confirm: None,
            options: Vec::new(),
        },
    );
    local.default_choice_id = Some("push_reply".into());
    let mut state = shell_with(vec![local]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);

    let input = state.handle_input_bytes(b"\r");
    assert!(matches!(
        endpoint_methods(&input)[..],
        [Method::WorkItemChoose(params)] if params.choice_id == "push_reply"
    ));
    assert!(state.overlay.is_none());
}

fn choice(id: &str, label: &str, action: WorkItemChoiceAction) -> WorkItemChoiceInfo {
    WorkItemChoiceInfo {
        choice_id: id.into(),
        label: label.into(),
        description: None,
        action,
        disabled_reason: None,
        confirm: None,
        options: Vec::new(),
    }
}

fn open_url(url: &str) -> WorkItemChoiceAction {
    WorkItemChoiceAction::OpenUrl { url: url.into() }
}

/// A Jira ticket with a workspace and the pull request ready to merge that is folded into it,
/// as the server projects them: the ticket offers the pull request's merges and its brief
/// under `pull_request:` ids, and the pull request's own item keeps every choice of its own.
fn ticket_with_folded_pull_request() -> (WorkItemInfo, WorkItemInfo) {
    let mut ticket = item("7");
    ticket.item_id = "jira:TECH-2073".into();
    ticket.source_id = "jira".into();
    ticket.context = "TECH-2073".into();
    ticket.title = "Add a LeadIn strategy decorator".into();
    ticket.url = "https://x.atlassian.net/browse/TECH-2073".into();
    ticket.tracker_state = Some("In Progress · Andrea Rossi".into());
    ticket.workspace_id = Some("ws_2".into());
    ticket.choices = vec![
        choice(
            "pull_request:merge_squash",
            "Squash and merge",
            WorkItemChoiceAction::Perform,
        ),
        choice(
            "pull_request:push_reply",
            "Ask agent to push and reply",
            WorkItemChoiceAction::BriefAgent,
        ),
        choice(
            "pull_request:merge_commit",
            "Create a merge commit",
            WorkItemChoiceAction::Perform,
        ),
        choice(
            "pull_request_open",
            "Open pull request",
            open_url("https://github.com/o/r/pull/11938"),
        ),
        choice(
            "jira",
            "Open in Jira",
            open_url("https://x.atlassian.net/browse/TECH-2073"),
        ),
    ];
    ticket.default_choice_id = Some("pull_request:merge_squash".into());
    let mut pull = item("11938");
    pull.item_id = "github:merge:o/r#11938".into();
    pull.context = "#11938 ready to merge · o/r".into();
    pull.folded_into = Some("jira:TECH-2073".into());
    let mut push = choice(
        "push_reply",
        "Ask agent to push and reply",
        WorkItemChoiceAction::BriefAgent,
    );
    push.disabled_reason = Some("Work on it locally first".into());
    pull.choices = vec![
        choice(
            "merge_squash",
            "Squash and merge",
            WorkItemChoiceAction::Perform,
        ),
        push,
        choice(
            "address_agent",
            "Ask agent to address the feedback",
            WorkItemChoiceAction::ProvisionWorkspace,
        ),
        choice(
            "address",
            "Work on it locally",
            WorkItemChoiceAction::ProvisionWorkspace,
        ),
        choice(
            "merge_commit",
            "Create a merge commit",
            WorkItemChoiceAction::Perform,
        ),
    ];
    pull.default_choice_id = Some("merge_squash".into());
    (ticket, pull)
}

/// The shell with the right-click menu of the first listed item open.
fn state_with_item_menu(items: Vec<WorkItemInfo>) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(two_workspace_snapshot()));
    state.set_pane_surface(surface());
    let mut projected = projection(1, items);
    projected.sources.push(WorkItemSourceInfo {
        source_id: "jira".into(),
        label: "Jira".into(),
        error: None,
    });
    state.set_endpoint_work_items(&ClientEndpointId::Local, projected);
    state.compose(106, 30).expect("frame");
    let rect = state.hits.work_items[0].rect;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Right),
        rect.x + 2,
        rect.y,
    );
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::ContextMenu(_))),
        "item menu opens"
    );
    state
}

/// The labels of the open menu's rows, and the row that is highlighted.
fn menu_labels(state: &ClientShellState) -> (Vec<String>, usize) {
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("a menu is open");
    };
    (
        menu.items()
            .iter()
            .map(|entry| entry.label.to_string())
            .collect(),
        menu.highlighted,
    )
}

#[test]
fn ticket_menu_leads_with_its_pull_requests_next_step_and_names_where_links_go() {
    let (ticket, pull) = ticket_with_folded_pull_request();
    let mut state = state_with_item_menu(vec![ticket, pull, item("9")]);

    let (labels, highlighted) = menu_labels(&state);
    // The ticket offers the merge itself, so its group leads and the merge is highlighted.
    assert_eq!(labels[0], "TECH-2073 · In Progress");
    assert_eq!(labels[highlighted], "Squash and merge");
    assert!(
        labels.contains(&"Open TECH-2073 in Jira".to_string()),
        "{labels:?}"
    );
    assert!(
        labels.contains(&"Open #11938 on GitHub".to_string()),
        "{labels:?}"
    );
    assert_eq!(labels.last().map(String::as_str), Some("Close workspace"));

    // Down skips the header and divider between the two groups.
    state.move_context_menu_selection(3);
    let (labels, highlighted) = menu_labels(&state);
    assert_eq!(labels[highlighted], "Ask agent to address the feedback");
}

#[test]
fn the_pull_request_group_leaves_out_what_the_ticket_carries_and_keeps_what_only_it_can_do() {
    let (ticket, pull) = ticket_with_folded_pull_request();
    let state = state_with_item_menu(vec![ticket, pull, item("9")]);

    let (labels, _) = menu_labels(&state);
    let count = |label: &str| labels.iter().filter(|row| *row == label).count();
    // The merge is the ticket's, listed once rather than again for the pull request.
    assert_eq!(count("Squash and merge"), 1, "{labels:?}");
    // Starting a workspace for the pull request item stays under its own header.
    assert!(
        labels.contains(&"#11938 ready to merge".to_string()),
        "{labels:?}"
    );
    assert_eq!(count("Work on it locally"), 1, "{labels:?}");
}

#[test]
fn a_pull_request_choice_that_only_reads_like_a_carried_one_is_still_listed() {
    let (ticket, mut pull) = ticket_with_folded_pull_request();
    // Another choice of the pull request item that happens to share a label with one the
    // ticket carries.
    pull.choices.insert(
        0,
        choice(
            "squash_elsewhere",
            "Squash and merge",
            WorkItemChoiceAction::Perform,
        ),
    );
    let state = state_with_item_menu(vec![ticket, pull, item("9")]);

    let (labels, _) = menu_labels(&state);
    let squashes = labels.iter().filter(|row| *row == "Squash and merge");
    assert_eq!(squashes.count(), 2, "{labels:?}");
}

#[test]
fn an_items_menu_leaves_out_the_choice_that_would_start_the_workspace_it_already_has() {
    let mut worked = item("7");
    worked.workspace_id = Some("ws_2".into());
    let state = state_with_item_menu(vec![worked]);

    let (labels, _) = menu_labels(&state);
    assert!(!labels.contains(&"Review".to_string()), "{labels:?}");
}

/// A review request as the server projects it: one Review choice with its two switches. The
/// server offers nothing that only opens the pull request in the browser; the menu's link does.
fn review_item(id: &str) -> WorkItemInfo {
    let mut review = item(id);
    review.seen = true;
    review.choices = vec![WorkItemChoiceInfo {
        options: vec![
            switch("worktree", "Create worktree", true),
            switch("post", "Post to GitHub", false),
        ],
        ..choice("review", "Review", WorkItemChoiceAction::ProvisionWorkspace)
    }];
    review.default_choice_id = Some("review".into());
    review
}

/// A review request whose default is another choice, so Review is not highlighted when the
/// dialog opens.
fn review_item_defaulting_to_open(id: &str) -> WorkItemInfo {
    let mut review = review_item(id);
    review.choices.push(choice(
        "open",
        "Open in the browser",
        open_url(&format!("https://github.com/o/r/pull/{id}")),
    ));
    review.default_choice_id = Some("open".into());
    review
}

fn switch(id: &str, label: &str, default: bool) -> WorkItemChoiceOptionInfo {
    WorkItemChoiceOptionInfo {
        option_id: id.into(),
        label: label.into(),
        description: None,
        default,
    }
}

/// The dialog of `item`, open on its default choice.
fn dialog_for(item: WorkItemInfo) -> ClientShellState {
    let mut state = shell_with(vec![item]);
    state.compose(106, 30).expect("frame");
    click_item(&mut state, 0);
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::WorkItem(_))),
        "the dialog opens"
    );
    state
}

/// The `options` of the one `work_item.choose` request in `input`.
fn chosen_options(input: &ClientShellInput) -> Option<Vec<String>> {
    let methods = endpoint_methods(input);
    let [Method::WorkItemChoose(params)] = methods[..] else {
        panic!("expected one choose request, got {methods:?}");
    };
    params.options.clone()
}

#[test]
fn dialog_shows_the_highlighted_choices_switches_and_numbers_flip_them() {
    let mut state = dialog_for(review_item("7"));
    let text = screen_text(&mut state);
    assert!(text.contains("1 [x] Create worktree"), "{text}");
    assert!(text.contains("2 [ ] Post to GitHub"), "{text}");
    assert!(text.contains("1-2 toggle"), "{text}");

    state.handle_input_bytes(b"2");
    state.handle_input_bytes(b"1");
    let text = screen_text(&mut state);
    assert!(text.contains("1 [ ] Create worktree"), "{text}");
    assert!(text.contains("2 [x] Post to GitHub"), "{text}");

    let input = state.handle_input_bytes(b"\r");
    assert_eq!(chosen_options(&input), Some(vec!["post".to_string()]));
}

#[test]
fn switches_left_alone_or_put_back_leave_the_defaults_to_the_server() {
    for keys in ["", "11"] {
        let mut state = dialog_for(review_item("7"));
        if !keys.is_empty() {
            state.handle_input_bytes(keys.as_bytes());
        }
        let input = state.handle_input_bytes(b"\r");
        assert_eq!(chosen_options(&input), None, "after pressing {keys:?}");
    }
}

#[test]
fn switching_every_option_off_sends_an_empty_list_rather_than_none() {
    let mut state = dialog_for(review_item("7"));
    state.handle_input_bytes(b"1");
    let input = state.handle_input_bytes(b"\r");
    assert_eq!(chosen_options(&input), Some(Vec::new()));
}

#[test]
fn a_choice_with_switches_takes_two_clicks() {
    // Another choice is the default, so the first click on Review only highlights it.
    let mut state = dialog_for(review_item_defaulting_to_open("7"));
    state.compose(106, 30).expect("frame");
    let (review_row, _) = state.hits.overlay_choice_rows[0];
    let first = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        review_row.x + 2,
        review_row.y,
    );
    assert!(
        endpoint_methods(&first).is_empty(),
        "the first click only highlights the choice"
    );

    state.compose(106, 30).expect("frame");
    let (review_row, _) = state.hits.overlay_choice_rows[0];
    let second = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        review_row.x + 2,
        review_row.y,
    );
    assert!(matches!(
        endpoint_methods(&second)[..],
        [Method::WorkItemChoose(params)] if params.choice_id == "review"
    ));
}

#[test]
fn clicking_a_switch_flips_it() {
    let mut state = dialog_for(review_item("7"));
    state.compose(106, 30).expect("frame");
    let (post_row, _) = state.hits.overlay_option_rows[1];
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        post_row.x + 2,
        post_row.y,
    );
    let input = state.handle_input_bytes(b"\r");
    assert_eq!(
        chosen_options(&input),
        Some(vec!["worktree".to_string(), "post".to_string()])
    );
}

#[test]
fn switches_belong_to_the_highlighted_choice_only() {
    let mut state = dialog_for(review_item_defaulting_to_open("7"));
    let text = screen_text(&mut state);
    assert!(
        !text.contains("Create worktree") && !text.contains("toggle"),
        "{text}"
    );

    // A number means nothing to a choice without switches, and flips nothing behind its back.
    state.handle_input_bytes(b"1");
    state.handle_input_bytes(b"\x1b[A");
    let text = screen_text(&mut state);
    assert!(text.contains("1 [x] Create worktree"), "{text}");
}

#[test]
fn a_review_request_is_still_opened_in_the_browser_from_its_menu() {
    // The server offers no choice that only opens a pull request; the menu's link does.
    let mut state = state_with_item_menu(vec![review_item("7")]);
    let (labels, _) = menu_labels(&state);
    let index = labels
        .iter()
        .position(|label| label == "Open #7 on GitHub")
        .unwrap_or_else(|| panic!("the pull request's link is listed: {labels:?}"));
    state.compose(106, 30).expect("frame");
    let (row, _) = state.hits.context_menu_rows[index];
    let input = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        row.x + 1,
        row.y,
    );
    assert!(
        input.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::OpenSafeWebUrl(url) if url == "https://github.com/o/r/pull/7"
        )),
        "the link opens the pull request"
    );
}

#[test]
fn a_choice_that_cannot_run_shows_no_switches() {
    let mut review = review_item("7");
    review.choices[0].disabled_reason = Some("No agent configured for o/r review requests".into());
    let mut state = dialog_for(review);
    let text = screen_text(&mut state);
    assert!(text.contains("No agent configured"), "{text}");
    assert!(!text.contains("Create worktree"), "{text}");
}

#[test]
fn flipped_switches_survive_a_fresh_projection_under_the_open_dialog() {
    let mut state = dialog_for(review_item("7"));
    state.handle_input_bytes(b"2");
    state.set_endpoint_work_items(
        &ClientEndpointId::Local,
        projection(2, vec![review_item("7")]),
    );
    let text = screen_text(&mut state);
    assert!(text.contains("2 [x] Post to GitHub"), "{text}");
}

#[test]
fn right_click_menu_runs_a_choice_with_its_switches_at_their_defaults() {
    let mut state = state_with_item_menu(vec![review_item("7")]);
    let (labels, _) = menu_labels(&state);
    let index = labels
        .iter()
        .position(|label| label == "Review")
        .expect("Review is listed");
    state.compose(106, 30).expect("frame");
    let (row, _) = state.hits.context_menu_rows[index];
    let input = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        row.x + 1,
        row.y,
    );
    assert_eq!(chosen_options(&input), None);
}
