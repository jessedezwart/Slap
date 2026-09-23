use super::*;

use std::sync::atomic::AtomicBool;

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
            disconnected: Arc::new(AtomicBool::new(false)),
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
            disconnected: Arc::new(AtomicBool::new(false)),
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
            disconnected: Arc::new(AtomicBool::new(false)),
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
            disconnected: Arc::new(AtomicBool::new(false)),
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
