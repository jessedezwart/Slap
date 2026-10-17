use super::*;

#[test]
fn songbird_wrapped_cookie_errors_are_fatal_but_other_errors_are_not() {
    for error in [
        AudioError::CookiesRequired,
        AudioError::Shared(Arc::new(AudioError::CookiesRequired)),
    ] {
        let error = songbird::tracks::PlayError::Create(Arc::new(
            songbird::input::AudioStreamError::Fail(Box::new(error)),
        ));
        assert!(requires_cookies(&error));
    }
    let error = songbird::tracks::PlayError::Create(Arc::new(
        songbird::input::AudioStreamError::Fail(Box::new(AudioError::NoOpusFormat)),
    ));
    assert!(!requires_cookies(&error));
}

#[tokio::test]
async fn deferred_cookie_failure_requests_warning_and_stop() {
    use songbird::tracks::{Track, TrackState};

    let mut driver = songbird::driver::Driver::default();
    let handle = driver.play(
        Track::new_with_data(
            songbird::input::File::new("unused-test-track.opus").into(),
            Arc::new(TrackMetadata::default()),
        )
        .pause(),
    );
    let state = TrackState {
        playing: PlayMode::Errored(songbird::tracks::PlayError::Create(Arc::new(
            songbird::input::AudioStreamError::Fail(Box::new(AudioError::CookiesRequired)),
        ))),
        ..TrackState::default()
    };
    let (commands, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let notifier = PlayerNotifier {
        guild_id: Id::new(1),
        event: TrackEvent::Error,
        updates: Arc::new(Notify::new()),
        commands,
        cancel: cancel.clone(),
        warning_channel_id: Id::new(20),
    };
    notifier
        .act(&EventContext::Track(&[(&state, &handle)]))
        .await;
    assert!(
        !cancel.is_cancelled(),
        "teardown must run outside the driver callback"
    );
    let VoiceCommand::CookiesRequired {
        guild_id,
        channel_id,
        ..
    } = receiver.try_recv().unwrap()
    else {
        panic!("expected warning and stop command");
    };
    assert_eq!(guild_id, Id::new(1));
    assert_eq!(channel_id, Id::new(20));
}
