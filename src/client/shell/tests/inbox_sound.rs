use super::*;

#[test]
fn inbox_notice_plays_the_inbox_sound_while_a_request_notice_keeps_the_request_sound() {
    let config = ClientShellConfig::from_config(&Config::default());
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let now = std::time::Instant::now();

    let (inbox_effects, _) = state.receive_inbox_notification(
        &ClientEndpointId::Local,
        "New pull request".into(),
        None,
        now,
    );
    let [ClientShellNotificationEffect::Sound {
        sound: inbox_sound, ..
    }] = inbox_effects.as_slice()
    else {
        panic!("expected exactly one inbox sound effect");
    };

    // A request-sound notification delivered immediately, as work-item notices were
    // before they had their own sound.
    let (request_effects, _) = state.receive_notification(
        &ClientEndpointId::Local,
        SemanticNotification {
            kind: SemanticNotificationKind::Custom,
            title: "needs input".into(),
            body: None,
            sound: Some(SemanticNotificationSound::Request),
            agent: None,
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            position: None,
        },
        now,
    );
    let [ClientShellNotificationEffect::Sound {
        sound: request_sound,
        ..
    }] = request_effects.as_slice()
    else {
        panic!("expected exactly one request sound effect");
    };

    assert_eq!(*inbox_sound, crate::sound::Sound::Inbox);
    assert_eq!(*request_sound, crate::sound::Sound::Request);
}
