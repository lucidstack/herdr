use super::work_items::{item, projection, two_workspace_snapshot};
use super::*;
use crate::api::schema::{Method, WorkItemInfo, WorkItemSourceInfo};

fn discovery_item(workspace_id: Option<&str>) -> WorkItemInfo {
    let mut discovery = item("pick");
    discovery.item_id = "github:pick-next".into();
    discovery.title = "Pick next \u{b7} GitHub".into();
    discovery.context = String::new();
    discovery.choices = Vec::new();
    discovery.default_choice_id = None;
    discovery.seen = true;
    discovery.workspace_id = workspace_id.map(str::to_string);
    discovery.is_pick_next = true;
    discovery
}

fn shell(snapshot: ClientShellSnapshot, items: Vec<WorkItemInfo>) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(super::surface());
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(1, items));
    state
}

fn screen_text(state: &mut ClientShellState) -> String {
    super::frame_rows(&state.compose(106, 30).expect("frame")).join("\n")
}

fn key(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        code,
        KeyModifiers::empty(),
    ))])
}

#[test]
fn discovery_workspace_nests_under_its_row_only_while_focused() {
    let mut state = shell(two_workspace_snapshot(), vec![discovery_item(Some("ws_2"))]);
    let text = screen_text(&mut state);
    assert!(text.contains("Pick next \u{b7} GitHub"), "{text}");
    // Collapsed, and never in the spaces list.
    assert!(!text.contains("review-space"), "{text}");

    let mut focused = two_workspace_snapshot();
    focused.workspaces[0].focused = false;
    focused.workspaces[1].focused = true;
    focused.focused_workspace_id = Some("ws_2".into());
    state.set_snapshot(Box::new(focused));
    let text = screen_text(&mut state);
    let lines: Vec<&str> = text.lines().collect();
    let row = lines
        .iter()
        .position(|line| line.contains("Pick next \u{b7} GitHub"))
        .expect("discovery row");
    assert_eq!(text.matches("review-space").count(), 1, "{text}");
    assert!(lines[row + 1].contains("review-space"), "{text}");
}

#[test]
fn pick_next_dialog_starts_the_chosen_provider_with_its_last_context() {
    let mut state = shell(two_workspace_snapshot(), vec![item("7")]);
    let mut two_sources = projection(2, vec![item("7")]);
    two_sources.sources.push(WorkItemSourceInfo {
        source_id: "jira".into(),
        label: "Jira".into(),
        error: None,
    });
    two_sources.pick_next.last_source_id = Some("jira".into());
    two_sources
        .pick_next
        .last_context
        .insert("github".into(), "open PRs".into());
    state.set_endpoint_work_items(&ClientEndpointId::Local, two_sources);
    let text = screen_text(&mut state);
    assert!(text.contains("+ Pick next task…"), "{text}");

    let row = state.hits.inbox.pick_next;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: row.x + 2,
        row: row.y,
        modifiers: KeyModifiers::NONE,
    })]);
    // The last provider is preselected; switching pre-fills that provider's last text.
    key(&mut state, KeyCode::Tab);
    let started = key(&mut state, KeyCode::Enter);
    let methods: Vec<&Method> = started
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(&request.method),
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            methods.as_slice(),
            [Method::WorkItemPickNextStart(params)]
                if params.source_id == "github" && params.context == "open PRs"
        ),
        "{methods:?}"
    );
}
