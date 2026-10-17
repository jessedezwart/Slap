use super::*;

use twilight_model::id::Id;

#[test]
fn queue_pages_are_bounded_and_keep_positions() {
    let tracks: Vec<_> = (0..13)
        .map(|i| {
            Arc::new(TrackMetadata {
                title: Some(format!("Song {i}")),
                ..TrackMetadata::default()
            })
        })
        .collect();
    let visible = std::iter::once(tracks[0].clone())
        .chain(tracks[6..11].iter().cloned())
        .collect::<Vec<_>>();
    let page = format_queue(&visible, tracks.len(), 2);
    assert!(page.contains("Current track:** Song 0"));
    assert!(page.contains("page 2/3"));
    assert!(page.contains("6. Song 6"));
    assert!(page.contains("10. Song 10"));
    assert!(!page.contains("11. Song 11"));
    assert!(format_queue(&tracks, tracks.len(), usize::MAX).contains("does not exist"));
    assert_eq!(format_queue(&[], 0, 1), "The queue is empty.");
    let metadata = Arc::new(TrackMetadata {
        title: Some("*@\n".repeat(100)),
        ..TrackMetadata::default()
    });
    let long = vec![metadata; 10];
    let message = format_queue(&long, long.len(), 1);
    assert!(message.chars().count() < 2000);
    assert_eq!(message.lines().count(), 8);
}

#[test]
fn formats_durations() {
    assert_eq!(format_duration(Duration::from_secs(5)), "0:05");
    assert_eq!(format_duration(Duration::from_secs(212)), "3:32");
    assert_eq!(format_duration(Duration::from_secs(3_725)), "1:02:05");
}

#[test]
fn escapes_markdown_and_mentions() {
    assert_eq!(escape("a*b_c`d"), "a\\*b\\_c\\`d");
    assert!(escape("@everyone").starts_with("@\u{200b}"));
}

#[test]
fn truncates_long_titles() {
    let long = "x".repeat(500);

    assert_eq!(escape(&long).chars().count(), 200);
}

#[test]
fn shared_cookie_errors_produce_actionable_command_response() {
    let error = PlayError::Audio(AudioError::Shared(Arc::new(AudioError::CookiesRequired)));
    assert_eq!(
        user_message(&error),
        AudioError::CookiesRequired.to_string()
    );
}

#[test]
fn modal_submissions_default_to_shuffling_and_appending() {
    use twilight_model::application::interaction::modal::{
        ModalInteractionLabel, ModalInteractionStringSelect, ModalInteractionTextInput,
    };

    let label = |component| {
        ModalInteractionComponent::Label(ModalInteractionLabel {
            id: 0,
            component: Box::new(component),
        })
    };
    let url = |value: &str| {
        label(ModalInteractionComponent::TextInput(
            ModalInteractionTextInput {
                custom_id: "url".into(),
                id: 1,
                value: value.into(),
            },
        ))
    };
    let select = |custom_id: &str, values: Vec<String>| {
        label(ModalInteractionComponent::StringSelect(
            ModalInteractionStringSelect {
                id: 2,
                custom_id: custom_id.into(),
                values,
            },
        ))
    };
    let shuffle = |values: Vec<String>| select("shuffle", values);
    let mode = |values: Vec<String>| select("mode", values);
    let data = |components| ModalInteractionData {
        components,
        custom_id: PLAY_MODAL.into(),
        resolved: None,
    };

    // An untouched select submits no value, which must keep shuffle on and
    // leave the import at the back of the queue.
    let input = play_input(&data(vec![url("  https://example.com/a  ")]));
    assert_eq!(input.url, "https://example.com/a");
    assert!(input.shuffle);
    assert_eq!(input.mode, QueueMode::Append);
    for (value, expected) in [
        ("append", QueueMode::Append),
        ("next", QueueMode::Next),
        ("replace", QueueMode::Replace),
        ("nonsense", QueueMode::Append),
    ] {
        let input = play_input(&data(vec![
            url("https://example.com/a"),
            mode(vec![value.into()]),
        ]));
        assert_eq!(input.mode, expected);
    }
    assert!(
        play_input(&data(vec![
            url("https://example.com/a"),
            shuffle(vec!["on".into()])
        ]))
        .shuffle
    );
    assert!(
        !play_input(&data(vec![
            url("https://example.com/a"),
            shuffle(vec!["off".into()])
        ]))
        .shuffle
    );
    // An empty link is rejected before anything is spawned.
    assert!(play_input(&data(vec![url("   ")])).url.is_empty());
}

#[test]
fn slash_options_default_to_shuffling_and_only_run_with_a_link() {
    use twilight_model::application::interaction::application_command::CommandDataOption;

    let data = |options: Vec<(&str, &str)>| CommandData {
        guild_id: None,
        id: Id::new(1),
        name: "play".into(),
        kind: CommandType::ChatInput,
        options: options
            .into_iter()
            .map(|(name, value)| CommandDataOption {
                name: name.into(),
                value: CommandOptionValue::String(value.into()),
            })
            .collect(),
        resolved: None,
        target_id: None,
    };

    // Without a link the command has nothing to run, so it opens the form.
    assert!(command_input(&data(vec![])).is_none());
    assert!(command_input(&data(vec![("shuffle", "off")])).is_none());

    let input = command_input(&data(vec![("url", "  https://example.com/a  ")])).unwrap();
    assert_eq!(input.url, "https://example.com/a");
    assert!(input.shuffle);
    assert_eq!(input.mode, QueueMode::Append);
    assert!(
        !command_input(&data(vec![
            ("url", "https://example.com/a"),
            ("shuffle", "off")
        ]))
        .unwrap()
        .shuffle
    );
    assert!(
        command_input(&data(vec![
            ("url", "https://example.com/a"),
            ("shuffle", "on")
        ]))
        .unwrap()
        .shuffle
    );
}
