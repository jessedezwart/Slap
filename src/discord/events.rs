//! The per-shard gateway loop.

use std::sync::Arc;

use tokio::sync::watch;
use tracing::{debug, warn};
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_gateway::{Event, EventTypeFlags, Shard, StreamExt as _};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, UserMarker},
};

use crate::discord::{commands, state::State};

/// Receive events for one shard until the shard closes or shutdown is signalled.
pub async fn dispatcher(state: Arc<State>, mut shard: Shard, mut shutdown: watch::Receiver<bool>) {
    loop {
        let item = tokio::select! {
            _ = shutdown.changed() => break,
            item = shard.next_event(EventTypeFlags::all()) => item,
        };

        let Some(item) = item else {
            break;
        };

        let event = match item {
            Ok(event) => event,
            Err(error) => {
                warn!(%error, "error receiving gateway event");

                continue;
            }
        };

        state.cache.update(&event);

        // songbird needs VoiceStateUpdate and VoiceServerUpdate to finish a
        // connection, so it sees every event before we do anything else.
        state.songbird.process(&event).await;

        handle_event(&state, event);
    }

    debug!(shard = %shard.id(), "shard dispatcher stopped");
}

fn handle_event(state: &Arc<State>, event: Event) {
    match event {
        Event::InteractionCreate(interaction) => {
            // Spawned so a slow yt-dlp resolve never holds up the gateway.
            tokio::spawn(commands::handle_interaction(
                Arc::clone(state),
                interaction.0,
            ));
        }
        Event::VoiceStateUpdate(update) => {
            let Some(guild_id) = update.guild_id else {
                return;
            };
            reconcile_voice(Arc::clone(state), guild_id);
        }
        Event::GatewayClose(frame) => {
            warn!(?frame, "gateway connection closed");
        }
        _ => {}
    }
}

/// Recheck the cache under the session lock, so queued gateway work cannot
/// disconnect a replacement session or race a join still in progress.
pub fn reconcile_voice(state: Arc<State>, guild_id: Id<GuildMarker>) {
    if state.sessions.channel_of(guild_id).is_none() {
        return;
    }
    tokio::spawn(async move {
        state
            .sessions
            .teardown_if(guild_id, "channel empty or bot disconnected/moved", || {
                let Some(channel_id) = state.sessions.channel_of(guild_id) else {
                    return false;
                };
                should_disconnect(&state.cache, guild_id, channel_id, state.user_id)
            })
            .await;
    });
}

fn should_disconnect(
    cache: &DefaultInMemoryCache,
    guild_id: Id<GuildMarker>,
    channel_id: Id<ChannelMarker>,
    bot_id: Id<UserMarker>,
) -> bool {
    let bot_channel = cache
        .voice_state(bot_id, guild_id)
        .map(|voice| voice.channel_id());
    bot_channel != Some(channel_id)
        || !cache
            .voice_channel_states(channel_id)
            .is_some_and(|mut voices| voices.any(|voice| voice.user_id() != bot_id))
}

#[cfg(test)]
mod tests {
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
}
