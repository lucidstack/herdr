use super::*;
use crate::api::schema::{
    Method, WorkItemChoiceAction, WorkItemChoiceInfo, WorkItemInfo, WorkItemPhase,
    WorkItemProvisioningInfo, WorkItemSourceInfo, WorkItemStep, WorkItemStepInfo,
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
        is_pick_next: false,
        start_reminder: None,
        running_choice_id: None,
        action_outcome: None,
        linked_pull_request: None,
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
    // The source still hears about the choice, so the item waits on GitHub.
    assert!(matches!(
        endpoint_methods(&input)[..],
        [Method::WorkItemChoose(params)] if params.choice_id == "github"
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
    let actions = menu
        .items()
        .iter()
        .position(|entry| entry.label == "Pull request actions...")
        .expect("folded pull request reachable");
    state.compose(106, 30).expect("frame");
    let (row, _) = state.hits.context_menu_rows[actions];
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        row.x + 1,
        row.y,
    );
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::WorkItem(overlay)) if overlay.item.item_id == "github:o/r#8"
    ));
}

#[test]
fn item_menu_of_a_started_item_opens_its_tracker_fix() {
    let mut started = item("7");
    started.seen = true;
    started.workspace_id = Some("ws_2".into());
    started.start_reminder = Some("You're working on this, but it isn't assigned to you".into());
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
    let update = menu
        .items()
        .iter()
        .position(|entry| entry.label == "Update ticket status...")
        .expect("tracker fix offered");
    state.compose(106, 30).expect("frame");
    let (row, _) = state.hits.context_menu_rows[update];
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        row.x + 1,
        row.y,
    );
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::WorkItem(overlay)) if overlay.item.item_id == "github:o/r#7"
    ));
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
