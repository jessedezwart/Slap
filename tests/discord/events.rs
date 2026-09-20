use super::*;
use twilight_cache_inmemory::ResourceType;

fn update(cache: &DefaultInMemoryCache, user: u64, channel: Option<u64>) {
    let payload = serde_json::json!({
        "guild_id": "1", "channel_id": channel.map(|id| id.to_string()),
        "user_id": user.to_string(), "session_id": "test", "deaf": false,
        "mute": false, "self_deaf": false, "self_mute": false,
        "self_video": false, "suppress": false
    });
    let update = serde_json::from_value(payload).unwrap();
    cache.update(&Event::VoiceStateUpdate(Box::new(update)));
}

#[test]
fn disconnects_only_when_alone_or_bot_moved_or_left() {
    let cache = DefaultInMemoryCache::builder()
        .resource_types(ResourceType::VOICE_STATE)
        .build();
    let check = || should_disconnect(&cache, Id::new(1), Id::new(10), Id::new(100));
    update(&cache, 100, Some(10));
    assert!(check(), "bot alone");
    update(&cache, 200, Some(10));
    assert!(!check(), "listener joined");
    update(&cache, 300, Some(20));
    assert!(!check(), "unrelated channel changed");
    update(&cache, 200, Some(20));
    assert!(check(), "last listener moved away");
    update(&cache, 200, Some(10));
    assert!(!check(), "listener returned before cleanup");
    update(&cache, 200, None);
    assert!(check(), "last listener disconnected");
    update(&cache, 200, Some(10));
    update(&cache, 100, Some(20));
    assert!(check(), "bot moved");
    update(&cache, 100, None);
    assert!(check(), "bot disconnected");
}
