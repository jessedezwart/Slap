use super::*;

/// All text in the view, in display order, including nested sections.
fn texts(components: &[Component]) -> String {
    components
        .iter()
        .map(|component| match component {
            Component::Container(container) => texts(&container.components),
            Component::Section(section) => texts(&section.components),
            Component::TextDisplay(display) => display.content.clone() + "\n",
            _ => String::new(),
        })
        .collect()
}

fn buttons_of(view: &View) -> Vec<&Button> {
    let Component::Container(container) = &view.components[0] else {
        panic!()
    };
    let Some(Component::ActionRow(row)) = container.components.last() else {
        panic!()
    };
    row.components
        .iter()
        .map(|component| match component {
            Component::Button(button) => button,
            _ => panic!(),
        })
        .collect()
}

#[test]
fn time_handles_unknown_zero_and_overrun() {
    assert_eq!(
        time(Duration::from_secs(10), Some(Duration::from_secs(300))),
        "`0:10 / 5:00`"
    );
    assert_eq!(
        time(Duration::from_secs(90), Some(Duration::from_secs(60))),
        "`1:00 / 1:00`"
    );
    for duration in [None, Some(Duration::ZERO)] {
        assert_eq!(time(Duration::from_secs(30), duration), "`0:30`");
    }
}

#[test]
fn paused_views_are_stable_and_titles_are_bounded() {
    let channel = Id::new(20);
    let metadata = TrackMetadata {
        title: Some("*@everyone\n".repeat(1000)),
        duration: Some(Duration::from_secs(60)),
        requester: Some(Id::new(30)),
        ..Default::default()
    };
    let info = TrackState {
        playing: PlayMode::Pause,
        position: Duration::from_secs(30),
        ready: ReadyState::Playable,
        ..Default::default()
    };
    let view = View::playing(&metadata, &info, 500, true, "", channel);
    let content = texts(&view.components);
    assert!(content.starts_with("### Paused"));
    assert!(!content.contains("@everyone"));
    assert!(content.contains("Added by <@30>"));
    assert!(content.contains("<#20>"));
    assert!(content.contains("`0:30 / 1:00` · Queue size: `500` · Shuffle: `On`"));
    assert!(content.chars().count() < 1000);
    assert_eq!(
        view,
        View::playing(&metadata, &info, 500, true, "", channel)
    );
    let buttons = buttons_of(&view);
    assert_eq!(buttons[0].custom_id.as_deref(), Some("player:resume"));
    assert_eq!(buttons[3].style, ButtonStyle::Primary);
    assert!(
        texts(&View::playing(&metadata, &TrackState::default(), 0, false, "", channel).components)
            .starts_with("### Loading…")
    );
}

#[test]
fn thumbnails_and_links_only_accept_https() {
    let metadata = TrackMetadata {
        title: Some("Song".into()),
        thumbnail: Some("https://i.ytimg.com/vi/x/hq.jpg".into()),
        webpage_url: Some("https://youtu.be/x".into()),
        ..Default::default()
    };
    let view = View::playing(&metadata, &TrackState::default(), 0, false, "", Id::new(1));
    let Component::Container(container) = &view.components[0] else {
        panic!()
    };
    assert!(
        container
            .components
            .iter()
            .any(|component| matches!(component, Component::Section(_)))
    );
    assert!(texts(&view.components).contains("**[Song](<https://youtu.be/x>)**"));
    let metadata = TrackMetadata {
        thumbnail: Some("javascript:alert(1)".into()),
        webpage_url: Some("http://example.com".into()),
        ..metadata
    };
    let view = View::playing(&metadata, &TrackState::default(), 0, false, "", Id::new(1));
    let Component::Container(container) = &view.components[0] else {
        panic!()
    };
    assert!(
        !container
            .components
            .iter()
            .any(|component| matches!(component, Component::Section(_)))
    );
    assert!(texts(&view.components).contains("**Song**"));
}

#[test]
fn controls_disable_for_empty_queues() {
    let view = View::empty(false, "");
    for (button, disabled) in buttons_of(&view)
        .into_iter()
        .zip([true, true, false, true, false])
    {
        assert_eq!(button.disabled, disabled);
        assert_eq!(button.style, ButtonStyle::Secondary);
        assert!(button.emoji.is_some());
        assert!(button_command(button.custom_id.as_deref().unwrap()).is_some());
    }
    assert!(
        texts(&View::empty(false, " Skipped 2 unavailable entries.").components)
            .contains("-# Skipped 2 unavailable entries.")
    );
    assert_eq!(button_command("player:destroy"), None);
}
