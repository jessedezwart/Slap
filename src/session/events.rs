//! songbird event handlers.

use std::sync::{Arc, Weak};

use songbird::{
    events::{Event, EventContext, EventHandler},
    tracks::PlayMode,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use twilight_model::id::{Id, marker::GuildMarker};

use super::{QueueEntry, SessionManager};

/// Turns any voice disconnect into a full teardown of the guild's pipeline.
pub struct DisconnectHandler {
    pub guild_id: Id<GuildMarker>,
    /// Weak reference (non-owning) so the handler does not keep the session manager alive.
    pub sessions: Weak<SessionManager>,
    /// The session this handler was attached for.
    pub cancel: CancellationToken,
}

#[async_trait::async_trait]
impl EventHandler for DisconnectHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        // On disconnect, teardown the session.
        if let EventContext::DriverDisconnect(data) = ctx
            // Take control of the session manager if it is still alive. 
            && let Some(sessions) = self.sessions.upgrade()
        {
            let reason = match data.reason {
                Some(reason) => format!("{:?} ({reason:?})", data.kind),
                None => format!("{:?}", data.kind),
            };
            let guild_id = self.guild_id;
            let cancel = self.cancel.clone();

            // Don't block the event handler to prevent a deadlock.
            tokio::spawn(async move {
                // Teardown is by guild, so a new session might be created after the disconnect.
                // Only teardown if the session is still the same one that was disconnected.
                sessions
                    .teardown_if(guild_id, &reason, |_| !cancel.is_cancelled())
                    .await;
            });
        }

        None
    }
}

pub struct TrackEventHandler {
    pub guild_id: Id<GuildMarker>,
    pub redraw: Arc<Notify>,
}

#[async_trait::async_trait]
impl EventHandler for TrackEventHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        // Notify the player task to redraw the view on every track event.
        self.redraw.notify_one();
        // Log errored tracks
        if let EventContext::Track(states) = ctx {
            for (state, handle) in *states {
                if let PlayMode::Errored(error) = &state.playing {
                    let entry = handle.data::<QueueEntry>();
                    let title = entry.metadata.display_title();
                    warn!(guild_id = %self.guild_id, title, %error, "track failed; queue will advance");
                }
            }
        }
        None
    }
}
