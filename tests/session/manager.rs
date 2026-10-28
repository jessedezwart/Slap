use super::*;

fn test_manager() -> Arc<SessionManager> {
    // No gateway or voice connection is opened in these lifecycle tests.
    let songbird = Songbird::twilight_from_config(
        Arc::new(songbird::shards::TwilightMap::new(Default::default())),
        Id::<UserMarker>::new(100),
        songbird::Config::default(),
    );
    SessionManager::new(Arc::new(songbird))
}

/// Install a session in `channel`, shown on the player `message`.
async fn insert(
    manager: &SessionManager,
    guild: Id<GuildMarker>,
    channel: Id<ChannelMarker>,
    message: Option<Id<MessageMarker>>,
) -> CancellationToken {
    let cancel = CancellationToken::new();
    *manager.lock(guild).await = Some(GuildSession {
        channel_id: channel,
        cancel: cancel.clone(),
        redraw: Arc::new(Notify::new()),
        player_component_id: message,
    });
    cancel
}

#[tokio::test]
async fn stop_rejects_other_channels_and_cancels_pending_work() {
    let manager = test_manager();
    let guild = Id::new(1);
    let channel = Id::new(10);
    let player = Id::new(100);
    let cancel = insert(&manager, guild, channel, Some(player)).await;
    let child = cancel.child_token();
    assert!(matches!(
        manager
            .control(guild, Id::new(20), player, Control::Stop)
            .await,
        Err(SessionError::WrongChannel)
    ));
    assert!(!child.is_cancelled());
    assert!(
        manager
            .control(guild, channel, player, Control::Stop)
            .await
            .is_ok()
    );
    assert!(child.is_cancelled());
    assert!(manager.lock(guild).await.is_none());

    // A late playlist result cannot append to the next voice session.
    let replacement = insert(&manager, guild, channel, None).await;
    assert!(matches!(
        manager
            .enqueue(guild, &cancel, vec![], QueueMode::Append)
            .await,
        Err(SessionError::Ended)
    ));
    manager
        .teardown_if(guild, "stale disconnect", |_| !cancel.is_cancelled())
        .await;
    assert!(!replacement.is_cancelled());
    assert!(manager.lock(guild).await.is_some());
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
    let player = Id::new(100);
    let cancel = insert(&manager, guild, channel, Some(player)).await;
    assert!(matches!(
        manager
            .control(guild, Id::new(20), player, Control::Stop)
            .await,
        Err(SessionError::WrongChannel)
    ));
    assert!(!cancel.is_cancelled());
    manager
        .control(guild, channel, player, Control::Stop)
        .await
        .unwrap();
    assert!(cancel.is_cancelled());

    // The replacement session shows its player on another message.
    let replacement = insert(&manager, guild, channel, Some(Id::new(200))).await;
    assert!(manager.now_playing(guild, 0, &cancel).await.is_none());
    assert!(
        manager
            .claim_player_component(guild, &cancel, Id::new(300))
            .await
            .is_none()
    );
    assert!(matches!(
        manager.control(guild, channel, player, Control::Stop).await,
        Err(SessionError::Ended)
    ));
    assert!(!replacement.is_cancelled());
    manager.teardown(guild, "test cleanup").await;
}

#[tokio::test]
async fn each_session_has_one_player() {
    let manager = test_manager();
    let guild = Id::new(1);
    let cancel = insert(&manager, guild, Id::new(10), None).await;

    assert!(
        manager
            .claim_player_component(guild, &cancel, Id::new(100))
            .await
            .is_some()
    );
    assert!(
        manager
            .claim_player_component(guild, &cancel, Id::new(200))
            .await
            .is_none(),
        "one player per session"
    );

    // Cleanup by another message must not release the current player.
    manager.release_player_component(guild, Id::new(200)).await;
    assert!(
        manager
            .claim_player_component(guild, &cancel, Id::new(300))
            .await
            .is_none()
    );

    // Once the player is gone, a fresh /play response takes over.
    manager.release_player_component(guild, Id::new(100)).await;
    assert!(
        manager
            .claim_player_component(guild, &cancel, Id::new(300))
            .await
            .is_some()
    );
    manager.teardown(guild, "test cleanup").await;
}
