//! The per-shard gateway loop.

use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use twilight_gateway::{CloseFrame, Event, EventTypeFlags, Shard, StreamExt as _};

use crate::{
    bot::BotContext,
    discord::{interactions, voice::reconcile_voice},
};

/// Receive events for one shard until the shard closes or shutdown is signalled.
pub async fn run_shard(state: Arc<BotContext>, mut shard: Shard, shutdown: CancellationToken) {
    loop {
        let item = tokio::select! {
            () = shutdown.cancelled() => {
                close(&mut shard).await;
                break;
            }
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

        state.discord_cache.update(&event);

        // songbird needs VoiceStateUpdate and VoiceServerUpdate to finish a
        // connection, so it sees every event before we do anything else.
        state.songbird.process(&event).await;

        handle_event(&state, event);
    }

    debug!(shard = %shard.id(), "shard stopped");
}

async fn close(shard: &mut Shard) {
    shard.close(CloseFrame::NORMAL);

    // Wait for the shard to close
    let closed = async {
        while let Some(item) = shard.next_event(EventTypeFlags::empty()).await {
            if let Ok(Event::GatewayClose(_)) = item {
                break;
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .is_err()
    {
        warn!(shard = %shard.id(), "gateway did not close in time");
    }
}

fn handle_event(state: &Arc<BotContext>, event: Event) {
    match event {
        Event::InteractionCreate(interaction) => {
            // Spawned so a slow yt-dlp resolve never holds up the gateway.
            tokio::spawn(interactions::handle_interaction(
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
