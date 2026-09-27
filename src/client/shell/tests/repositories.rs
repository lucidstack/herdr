use super::work_items::{item, projection, two_workspace_snapshot};
use super::*;
use crate::api::schema::{Method, WorkItemRepositoryInfo};

fn repository(home: Option<&str>) -> WorkItemRepositoryInfo {
    WorkItemRepositoryInfo {
        path: "/src/app".into(),
        label: "app".into(),
        workspace_id: home.map(str::to_string),
    }
}

/// `ws_2` ("review-space"), optionally focused.
fn snapshot_with_focus(focused: bool) -> ClientShellSnapshot {
    let mut snapshot = two_workspace_snapshot();
    if focused {
        snapshot.workspaces[0].focused = false;
        snapshot.workspaces[1].focused = true;
        snapshot.focused_workspace_id = Some("ws_2".into());
    }
    snapshot
}

fn shell(snapshot: ClientShellSnapshot, home: Option<&str>) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(super::surface());
    let mut projection = projection(1, vec![item("7")]);
    projection.repositories = vec![repository(home)];
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection);
    state
}

fn screen_text(state: &mut ClientShellState) -> String {
    super::frame_rows(&state.compose(106, 30).expect("frame")).join("\n")
}

fn click_repository(state: &mut ClientShellState) -> Vec<Method> {
    let (rect, _) = state.hits.inbox.repositories[0];
    let input = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + 2,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    })]);
    input
        .actions
        .into_iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(request.method),
            _ => None,
        })
        .collect()
}

#[test]
fn repository_home_nests_under_its_row_only_while_focused() {
    let mut state = shell(snapshot_with_focus(false), Some("ws_2"));
    let text = screen_text(&mut state);
    assert!(text.contains(" repositories"), "{text}");
    // Collapsed, and never in the spaces list.
    assert!(!text.contains("review-space"), "{text}");

    state.set_snapshot(Box::new(snapshot_with_focus(true)));
    let text = screen_text(&mut state);
    let lines: Vec<&str> = text.lines().collect();
    let row = usize::from(state.hits.inbox.repositories[0].0.y);
    assert!(lines[row].contains("app"), "{text}");
    assert!(lines[row + 1].contains("review-space"), "{text}");
    assert_eq!(text.matches("review-space").count(), 1, "{text}");
}

#[test]
fn repository_row_focuses_its_open_checkout_or_opens_one() {
    let mut state = shell(snapshot_with_focus(false), Some("ws_2"));
    screen_text(&mut state);
    assert!(
        matches!(
            click_repository(&mut state).as_slice(),
            [Method::WorkspaceFocus(target)] if target.workspace_id == "ws_2"
        ),
        "focuses the open main checkout"
    );

    let mut state = shell(two_workspace_snapshot(), None);
    screen_text(&mut state);
    assert!(
        matches!(
            click_repository(&mut state).as_slice(),
            [Method::WorkspaceCreate(params)]
                if params.cwd.as_deref() == Some("/src/app") && params.focus
        ),
        "opens a workspace on the clone"
    );
}
