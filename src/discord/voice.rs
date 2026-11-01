//! Leaving a voice channel once nobody is listening.

use std::sync::Arc;

use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, UserMarker},
};

use crate::bot::BotContext;

/// Recheck the cache under the session lock, so queued gateway work cannot
/// disconnect a replacement session or race a join still in progress.
pub fn reconcile_voice(state: Arc<BotContext>, guild_id: Id<GuildMarker>) {
    if !state.sessions.is_known_guild(guild_id) {
        return;
    }
    tokio::spawn(async move {
        state
            .sessions
            .teardown_if(
                guild_id,
                "channel empty or bot disconnected/moved",
                |session| {
                    should_disconnect(
                        &state.discord_cache,
                        guild_id,
                        session.channel_id,
                        state.user_id,
                    )
                },
            )
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
#[path = "../../tests/discord/voice.rs"]
mod tests;
