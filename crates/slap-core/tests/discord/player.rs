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
        playlist: Some(Arc::new(Playlist {
            name: "Late Night".into(),
            url: "https://www.youtube.com/playlist?list=PL1".parse().unwrap(),
        })),
        ..Default::default()
    };
    let info = TrackState {
        playing: PlayMode::Pause,
        position: Duration::from_secs(30),
        ready: ReadyState::Playable,
        ..Default::default()
    };
    let view = View::playing(&metadata, &info, 500, "", channel);
    let content = texts(&view.components);
    assert!(content.starts_with("### Paused"));
    assert!(!content.contains("@everyone"));
    assert!(content.contains("From **[Late Night](<https://www.youtube.com/playlist?list=PL1>)**"));
    assert!(content.contains("Added by <@30>"));
    assert!(content.contains("<#20>"));
    assert!(content.contains("`0:30 / 1:00` · Queue size: `500`"));
    assert!(content.chars().count() < 1000);
    assert_eq!(view, View::playing(&metadata, &info, 500, "", channel));
    let buttons = buttons_of(&view);
    assert_eq!(buttons[0].custom_id.as_deref(), Some("player:resume"));
    assert!(
        texts(&View::playing(&metadata, &TrackState::default(), 0, "", channel).components)
            .starts_with("### Loading…")
    );
}

#[test]
fn thumbnails_and_links_only_accept_https() {
    let metadata = TrackMetadata {
        title: Some("Song".into()),
        thumbnail: Some("https://i.ytimg.com/vi/x/hq.jpg".into()),
        webpage_url: Some("https://youtu.be/x".parse().unwrap()),
        ..Default::default()
    };
    let view = View::playing(&metadata, &TrackState::default(), 0, "", Id::new(1));
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
        webpage_url: Some("http://example.com".parse().unwrap()),
        ..metadata
    };
    let view = View::playing(&metadata, &TrackState::default(), 0, "", Id::new(1));
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
    let view = View::empty("");
    for (button, disabled) in buttons_of(&view)
        .into_iter()
        .zip([true, true, false, false, false])
    {
        assert_eq!(button.disabled, disabled);
        assert_eq!(button.style, ButtonStyle::Secondary);
        assert!(button.emoji.is_some());
        assert!(button_command(button.custom_id.as_deref().unwrap()).is_some());
    }
    assert!(
        texts(&View::empty(" Skipped 2 unavailable entries.").components)
            .contains("-# Skipped 2 unavailable entries.")
    );
    assert_eq!(button_command("player:destroy"), None);
    assert_eq!(button_command("player:shuffle"), None);
    assert_eq!(button_command("player:add"), Some("play"));
}

#[test]
fn stopped_players_offer_the_link_again() {
    let replay = Replay {
        url: "https://example.com/a`b".into(),
        shuffle: false,
        playlist: None,
    };
    let view = View::stopped(&replay, false);
    let content = texts(&view.components);
    assert!(content.starts_with("### Playback stopped"));
    assert!(content.contains("Press **Play again**"));
    // Backticks would end the code span the command is copied from.
    assert!(content.contains("`/play url: https://example.com/ab shuffle: Off`"));
    let buttons = buttons_of(&view);
    assert_eq!(buttons.len(), 1);
    assert_eq!(buttons[0].custom_id.as_deref(), Some("player:again"));
    assert!(!buttons[0].disabled);
    assert_eq!(button_command("player:again"), Some("play-again"));
}

#[test]
fn disconnected_players_cannot_be_replayed_but_still_show_the_command() {
    let replay = Replay {
        url: "https://example.com/a".into(),
        shuffle: true,
        // Only HTTPS links are embedded; anything else stays a plain label.
        playlist: Some(Arc::new(Playlist {
            name: "*Road* Trip".into(),
            url: "http://example.com/list".parse().unwrap(),
        })),
    };
    let view = View::stopped(&replay, true);
    let content = texts(&view.components);
    assert!(!content.contains("Play again**"));
    assert!(content.contains("`/play url: https://example.com/a shuffle: On`"));
    // The name is a label, not markup.
    assert!(content.contains("**\\*Road\\* Trip**"));
    assert!(buttons_of(&view)[0].disabled);
}
