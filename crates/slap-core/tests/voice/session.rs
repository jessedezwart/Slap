use super::*;

fn test_manager() -> Arc<SessionManager> {
    // No gateway or voice connection is opened in these lifecycle tests.
    let songbird = Songbird::twilight_from_config(
        Arc::new(songbird::shards::TwilightMap::new(Default::default())),
        Id::<twilight_model::id::marker::UserMarker>::new(100),
        songbird::Config::default(),
    );
    SessionManager::new(Arc::new(songbird)).0
}

#[tokio::test]
async fn stop_rejects_other_channels_and_cancels_pending_work() {
    let manager = test_manager();
    let guild = Id::new(1);
    let channel = Id::new(10);
    let cancel = CancellationToken::new();
    let child = cancel.child_token();
    manager.sessions.insert(
        guild,
        GuildSession {
            channel_id: channel,
            cancel: cancel.clone(),
            updates: Arc::new(Notify::new()),
        },
    );
    assert!(matches!(
        manager
            .control(guild, Id::new(20), None, Control::Stop)
            .await,
        Err(SessionError::WrongChannel)
    ));
    assert!(!child.is_cancelled());
    assert!(
        manager
            .control(guild, channel, None, Control::Stop)
            .await
            .is_ok()
    );
    assert!(child.is_cancelled());
    assert_eq!(manager.channel_of(guild), None);

    // A late playlist result cannot append to the next voice session.
    let replacement = CancellationToken::new();
    manager.sessions.insert(
        guild,
        GuildSession {
            channel_id: channel,
            cancel: replacement.clone(),
            updates: Arc::new(Notify::new()),
        },
    );
    assert!(matches!(
        manager
            .enqueue(guild, &cancel, vec![], QueueMode::Append)
            .await,
        Err(SessionError::NotConnected)
    ));
    manager
        .teardown_if(guild, "stale disconnect", || !cancel.is_cancelled())
        .await;
    assert!(!replacement.is_cancelled());
    assert_eq!(manager.channel_of(guild), Some(channel));
    manager.teardown(guild, "test cleanup").await;
}

#[test]
fn playing_next_keeps_the_current_track_and_the_import_order() {
    let mut queue: VecDeque<u32> = VecDeque::from([0, 1, 2, 10, 11, 12]);
    play_next(&mut queue, 3);
    assert_eq!(queue, VecDeque::from([0, 10, 11, 12, 1, 2]));

    // A queue that only holds the import has nothing to jump ahead of.
    let mut fresh: VecDeque<u32> = VecDeque::from([10, 11]);
    play_next(&mut fresh, 2);
    assert_eq!(fresh, VecDeque::from([10, 11]));

    // The import already sits directly behind the current track.
    let mut behind: VecDeque<u32> = VecDeque::from([0, 10]);
    play_next(&mut behind, 1);
    assert_eq!(behind, VecDeque::from([0, 10]));

    play_next(&mut VecDeque::<u32>::new(), 0);
}

#[tokio::test]
async fn controls_from_an_ended_session_cannot_stop_its_replacement() {
    let manager = test_manager();
    let guild = Id::new(1);
    let channel = Id::new(10);
    let cancel = CancellationToken::new();
    manager.sessions.insert(
        guild,
        GuildSession {
            channel_id: channel,
            cancel: cancel.clone(),
            updates: Arc::new(Notify::new()),
        },
    );
    assert!(manager.session(guild, &cancel).await.is_some());
    assert!(matches!(
        manager
            .control(guild, Id::new(20), Some(&cancel), Control::Stop)
            .await,
        Err(SessionError::WrongChannel)
    ));
    assert!(!cancel.is_cancelled());
    manager
        .control(guild, channel, Some(&cancel), Control::Stop)
        .await
        .unwrap();
    assert!(cancel.is_cancelled());

    let replacement = CancellationToken::new();
    manager.sessions.insert(
        guild,
        GuildSession {
            channel_id: channel,
            cancel: replacement.clone(),
            updates: Arc::new(Notify::new()),
        },
    );
    assert!(manager.session(guild, &cancel).await.is_none());
    assert!(manager.player_snapshot(guild, &cancel).await.is_none());
    assert!(matches!(
        manager
            .control(guild, channel, Some(&cancel), Control::Stop)
            .await,
        Err(SessionError::Ended)
    ));
    assert!(!replacement.is_cancelled());
    manager.teardown(guild, "test cleanup").await;
}
