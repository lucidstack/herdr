use super::work_items::{item, projection, two_workspace_snapshot};
use super::*;

fn config_with_sections(sections_toml: &str) -> ClientShellConfig {
    let config: Config =
        toml::from_str(&format!("[ui.sidebar]\nsections = {sections_toml}\n")).expect("config");
    ClientShellConfig::from_config(&config)
}

fn shell_with_sections(
    sections_toml: &str,
    items: Vec<crate::api::schema::WorkItemInfo>,
) -> ClientShellState {
    let mut state = ClientShellState::new(config_with_sections(sections_toml));
    state.set_snapshot(Box::new(two_workspace_snapshot()));
    state.set_pane_surface(super::surface());
    state.set_endpoint_work_items(&ClientEndpointId::Local, projection(1, items));
    state
}

fn screen_text(state: &mut ClientShellState) -> String {
    super::frame_rows(&state.compose(106, 30).expect("frame")).join("\n")
}

fn ten_items() -> Vec<crate::api::schema::WorkItemInfo> {
    (1..=10).map(|n| item(&n.to_string())).collect()
}

#[test]
fn hiding_spaces_gives_the_inbox_the_full_flow_height() {
    // With every default section, the inbox shares its zone with the spaces list
    // below it and can only show a bounded number of the ten pending items.
    let mut with_spaces = shell_with_sections(r#"["inbox", "spaces", "agents"]"#, ten_items());
    let text_with_spaces = screen_text(&mut with_spaces);
    let shown_with_spaces = with_spaces.hits.inbox.shown;
    assert!(text_with_spaces.contains(" spaces"), "{text_with_spaces}");
    assert!(with_spaces.hits.workspace_body.height > 0);
    assert!(shown_with_spaces < 10, "{text_with_spaces}");

    // Hiding "spaces" gives that reclaimed height to the inbox: it shows more of
    // the same ten items, and the spaces list draws no rows or hit targets.
    let mut without_spaces = shell_with_sections(r#"["inbox", "agents"]"#, ten_items());
    let text_without_spaces = screen_text(&mut without_spaces);
    assert!(
        !text_without_spaces.contains(" spaces"),
        "{text_without_spaces}"
    );
    assert_eq!(without_spaces.hits.workspace_body, Rect::default());
    assert!(without_spaces.hits.workspaces.is_empty());
    assert!(
        without_spaces.hits.inbox.shown > shown_with_spaces,
        "shown with spaces: {shown_with_spaces}, shown without: {} ({text_without_spaces})",
        without_spaces.hits.inbox.shown
    );
}

#[test]
fn sidebar_section_order_places_spaces_above_the_inbox_when_listed_first() {
    let mut state = shell_with_sections(r#"["spaces", "inbox", "agents"]"#, vec![item("7")]);
    let text = screen_text(&mut state);
    let spaces_row = text
        .lines()
        .position(|line| line.contains(" spaces"))
        .expect("spaces header");
    let inbox_row = text
        .lines()
        .position(|line| line.contains(" inbox"))
        .expect("inbox header");
    assert!(spaces_row < inbox_row, "{text}");
}

#[test]
fn sidebar_section_order_places_inbox_above_spaces_by_default() {
    let mut state = shell_with_sections(r#"["inbox", "spaces", "agents"]"#, vec![item("7")]);
    let text = screen_text(&mut state);
    let inbox_row = text
        .lines()
        .position(|line| line.contains(" inbox"))
        .expect("inbox header");
    let spaces_row = text
        .lines()
        .position(|line| line.contains(" spaces"))
        .expect("spaces header");
    assert!(inbox_row < spaces_row, "{text}");
}

#[test]
fn unknown_sidebar_section_is_a_config_diagnostic() {
    let result = toml::from_str::<Config>("[ui.sidebar]\nsections = [\"inbox\", \"bogus\"]\n");
    assert!(result.is_err());
}
