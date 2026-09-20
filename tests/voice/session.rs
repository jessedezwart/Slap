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
            panel_message: None,
            shuffle: false,
        },
    );
    assert!(matches!(
        manager.control(guild, Id::new(20), "stop", 1, None).await,
        Err(SessionError::WrongChannel)
    ));
    assert!(!child.is_cancelled());
    assert!(
        manager
            .control(guild, channel, "stop", 1, None)
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
            panel_message: None,
            shuffle: false,
        },
    );
    assert!(matches!(
        manager.enqueue(guild, &cancel, vec![]).await,
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
fn shuffle_preserves_current_and_all_upcoming_tracks() {
    let mut queue: VecDeque<_> = (0..100).collect();
    assert_eq!(shuffle_upcoming(&mut queue), 99);
    assert_eq!(queue.front(), Some(&0));
    let mut items: Vec<_> = queue.into_iter().collect();
    items.sort_unstable();
    assert_eq!(items, (0..100).collect::<Vec<_>>());
    assert_eq!(shuffle_upcoming(&mut VecDeque::<u8>::new()), 0);
    assert_eq!(shuffle_upcoming(&mut VecDeque::from([1])), 0);
    let mut two = VecDeque::from([1, 2]);
    assert_eq!(shuffle_upcoming(&mut two), 1);
    assert_eq!(two, VecDeque::from([1, 2]));
}

#[test]
fn restoring_order_keeps_the_current_track_first() {
    let mut queue: VecDeque<u64> = (0..100).collect();
    queue.push_front(500);
    shuffle_upcoming(&mut queue);
    restore_order(&mut queue, |sequence| *sequence);
    assert_eq!(queue.pop_front(), Some(500));
    assert!(queue.iter().copied().eq(0..100));
    restore_order(&mut VecDeque::<u64>::new(), |sequence| *sequence);
}

#[tokio::test]
async fn panel_ownership_is_unique_and_old_buttons_cannot_stop_replacements() {
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
            panel_message: None,
            shuffle: false,
        },
    );
    let (first, second) = tokio::join!(
        manager.attach_panel(guild, &cancel, Id::new(100)),
        manager.attach_panel(guild, &cancel, Id::new(200)),
    );
    assert_ne!(
        first.is_some(),
        second.is_some(),
        "exactly one updater wins"
    );
    let message = first.or(second).unwrap().panel_message.unwrap();
    assert!(matches!(
        manager
            .control(guild, Id::new(20), "stop", 1, Some(message))
            .await,
        Err(SessionError::WrongChannel)
    ));
    assert!(!cancel.is_cancelled());
    manager
        .control(guild, channel, "stop", 1, Some(message))
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
            panel_message: None,
            shuffle: false,
        },
    );
    assert!(
        manager
            .attach_panel(guild, &cancel, Id::new(300))
            .await
            .is_none()
    );
    assert!(
        manager
            .attach_panel(guild, &replacement, Id::new(400))
            .await
            .is_some()
    );
    // Cleanup by the previous worker must not release the new panel.
    manager.release_panel(guild, message);
    assert_eq!(
        manager.sessions.get(&guild).unwrap().panel_message,
        Some(Id::new(400))
    );
    assert!(matches!(
        manager
            .control(guild, channel, "stop", 1, Some(message))
            .await,
        Err(SessionError::StalePanel)
    ));
    assert!(!replacement.is_cancelled());

    // Deleting a panel permits a fresh /play response to take ownership.
    manager.release_panel(guild, Id::new(400));
    assert!(
        manager
            .attach_panel(guild, &replacement, Id::new(500))
            .await
            .is_some()
    );
    manager.teardown(guild, "test cleanup").await;
}
