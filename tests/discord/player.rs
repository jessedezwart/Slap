use super::*;

#[test]
fn buttons_read_back_as_their_actions() {
    for button in &BUTTONS {
        assert_eq!(button_command(button.custom_id), Some(button.action));
    }
    assert_eq!(button_command("player:destroy"), None);
    assert_eq!(button_command("player:shuffle"), None);
    assert_eq!(button_command("player:add"), Some(Action::Play));
    assert_eq!(
        button_command("player:stop"),
        Some(Action::Control(Control::Stop))
    );
}

#[test]
fn panel_ownership_is_unique_per_session() {
    let panels = Panels::default();
    let guild = Id::new(1);
    let cancel = CancellationToken::new();
    assert!(panels.claim(guild, &cancel, Id::new(100)));
    assert!(
        !panels.claim(guild, &cancel, Id::new(200)),
        "one player per session"
    );
    assert!(panels.session(guild, Id::new(100)).is_some());
    assert!(panels.session(guild, Id::new(200)).is_none());

    // An ended session's player gives way to the next session's.
    cancel.cancel();
    let replacement = CancellationToken::new();
    assert!(!panels.claim(guild, &cancel, Id::new(300)));
    assert!(panels.claim(guild, &replacement, Id::new(400)));

    // Cleanup by the previous player must not release the new one.
    panels.release(guild, Id::new(100));
    assert!(panels.session(guild, Id::new(400)).is_some());

    // Deleting a player lets a fresh /play response take ownership.
    panels.release(guild, Id::new(400));
    assert!(panels.claim(guild, &replacement, Id::new(500)));
}
