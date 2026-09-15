use super::*;
use crate::protocol::{ClientShellService, ClientShellServiceLiveness};

fn snapshot_with_services() -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.workspaces[0].services = vec![
        ClientShellService {
            id: 1,
            label: "Rails".into(),
            url: "http://localhost:3000".into(),
            liveness: ClientShellServiceLiveness::Up,
        },
        ClientShellService {
            id: 2,
            label: "Vite".into(),
            url: "http://127.0.0.1:5173/app".into(),
            liveness: ClientShellServiceLiveness::Down,
        },
    ];
    snapshot
}

fn frame_text(frame: &FrameData) -> String {
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn click(state: &mut ClientShellState, column: u16, row: u16) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })])
}

#[test]
fn services_start_collapsed_behind_a_count_chip() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot_with_services()));
    state.set_pane_surface(surface());
    let frame = state.compose(106, 20).expect("composed frame");
    let text = frame_text(&frame);
    assert!(text.contains("▸2"), "collapsed chip missing:\n{text}");
    assert!(
        !text.contains("Rails"),
        "services rendered while collapsed:\n{text}"
    );
    assert!(state.hits.services.is_empty());
    assert!(state.hits.workspaces[0].services_toggle.is_some());
}

#[test]
fn clicking_the_chip_expands_services_and_clicking_a_row_opens_its_url() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot_with_services()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");
    let chip = state.hits.workspaces[0]
        .services_toggle
        .expect("services chip");

    let toggled = click(&mut state, chip.x, chip.y);
    assert!(
        toggled.actions.is_empty(),
        "chip click must not focus the workspace"
    );
    let frame = state.compose(106, 20).expect("expanded frame");
    let text = frame_text(&frame);
    assert!(text.contains("▾2"), "expanded chip missing:\n{text}");
    assert!(text.contains("Rails"), "service label missing:\n{text}");
    assert!(text.contains(":3000"), "shortened url missing:\n{text}");
    assert!(
        text.contains("Vite  127.0.0.1:5"),
        "url is clipped to the sidebar:\n{text}"
    );
    assert_eq!(state.hits.services.len(), 2);
    let workspace = state.hits.workspaces[0].rect;
    let up_row = state.hits.services[0].rect;
    let down_row = state.hits.services[1].rect;
    assert_eq!(up_row.y, workspace.bottom());
    assert_eq!(down_row.y, up_row.y + 1);
    let dot = |rect: ratatui::layout::Rect| {
        frame.cells[usize::from(rect.y) * usize::from(frame.width) + usize::from(rect.x) + 3].fg
    };
    assert_eq!(
        dot(up_row),
        crate::protocol::color_to_u32(state.config.palette.green)
    );
    assert_eq!(
        dot(down_row),
        crate::protocol::color_to_u32(state.config.palette.red)
    );

    let opened = click(&mut state, down_row.x + 5, down_row.y);
    assert!(matches!(
        &opened.actions[..],
        [ClientShellAction::OpenSafeWebUrl(url)] if url == "http://127.0.0.1:5173/app"
    ));
    assert!(state.workspace_press.is_none());

    click(&mut state, chip.x, chip.y);
    state.compose(106, 20).expect("collapsed again");
    assert!(state.hits.services.is_empty());
}

#[test]
fn services_below_a_workspace_shift_following_rows() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut snapshot = snapshot_with_services();
    let mut second = snapshot.workspaces[0].clone();
    second.workspace_id = "ws_2".into();
    second.label = "second".into();
    second.focused = false;
    second.services = Vec::new();
    snapshot.workspaces.push(second);
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());
    state.compose(106, 24).expect("composed frame");
    let collapsed_second_y = state.hits.workspaces[1].rect.y;
    state
        .expanded_services
        .insert((ClientEndpointId::Local, "ws_1".into()));
    state.compose(106, 24).expect("expanded frame");
    assert_eq!(state.hits.workspaces[1].rect.y, collapsed_second_y + 2);
}
