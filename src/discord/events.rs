//! The per-shard gateway loop.

use std::sync::Arc;

use tokio::sync::watch;
use tracing::{debug, warn};
use twilight_gateway::{Event, EventTypeFlags, Shard, StreamExt as _};

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
            if update.user_id != state.user_id {
                return;
            }

            let Some(guild_id) = update.guild_id else {
                return;
            };

            // We were disconnected or dragged into another channel: the
            // pipeline that was feeding the old connection must go.
            let moved_away = update
                .channel_id
                .is_none_or(|channel_id| state.sessions.channel_of(guild_id) != Some(channel_id));

            if moved_away {
                let state = Arc::clone(state);

                tokio::spawn(async move {
                    state
                        .sessions
                        .teardown(guild_id, "voice state changed")
                        .await;
                });
            }
        }
        Event::GatewayClose(frame) => {
            warn!(?frame, "gateway connection closed");
        }
        _ => {}
    }
}
