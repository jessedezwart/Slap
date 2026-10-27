//! songbird event handlers.
//!
//! These run on songbird's driver task, so they must never reach back into the
//! `Call` they are attached to — that would re-enter a lock songbird is already
//! holding. They post notifications, spawn work and write bounded,
//! non-blocking logs.

use std::sync::{Arc, Weak};

use songbird::{
    events::{Event, EventContext, EventHandler, TrackEvent},
    tracks::{PlayMode, ReadyState},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use twilight_model::id::{Id, marker::GuildMarker};

use crate::voice::session::{QueueEntry, SessionManager};

/// Turns any voice disconnect into a full teardown of the guild's pipeline.
pub struct DriverDisconnectNotifier {
    pub guild_id: Id<GuildMarker>,
    /// Weak, because the manager owns songbird, which owns this handler.
    pub sessions: Weak<SessionManager>,
    /// The session this handler was attached for.
    pub cancel: CancellationToken,
}

#[async_trait::async_trait]
impl EventHandler for DriverDisconnectNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::DriverDisconnect(data) = ctx
            && let Some(sessions) = self.sessions.upgrade()
        {
            let reason = match data.reason {
                Some(reason) => format!("{:?} ({reason:?})", data.kind),
                None => format!("{:?}", data.kind),
            };
            let guild_id = self.guild_id;
            let cancel = self.cancel.clone();

            // Off the driver task: teardown locks the `Call`.
            tokio::spawn(async move {
                // A stale disconnect must not end a newer session.
                sessions
                    .teardown_if(guild_id, &reason, |_| !cancel.is_cancelled())
                    .await;
            });
        }

        None
    }
}

/// Wake the panel and log transitions. No queue locks, HTTP requests, or
/// per-frame logs here.
pub struct PlayerNotifier {
    pub guild_id: Id<GuildMarker>,
    pub event: TrackEvent,
    pub redraw: Arc<Notify>,
}

#[async_trait::async_trait]
impl EventHandler for PlayerNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        self.redraw.notify_one();
        if let EventContext::Track(states) = ctx {
            for (state, handle) in *states {
                let entry = handle.data::<QueueEntry>();
                let metadata = &entry.metadata;
                let span = tracing::info_span!("track", guild_id = %self.guild_id,
                    track_id = %handle.uuid(), title = ?metadata.title.as_deref().unwrap_or("Unknown track"));
                // No awaits inside the span guard on this driver callback.
                let _entered = span.enter();
                match self.event {
                    TrackEvent::Play | TrackEvent::Playable
                        if state.playing == PlayMode::Play
                            && state.ready == ReadyState::Playable =>
                    {
                        info!(position_secs = state.position.as_secs(), "now playing");
                    }
                    TrackEvent::Pause if state.ready == ReadyState::Playable => {
                        info!(position_secs = state.position.as_secs(), "paused");
                    }
                    TrackEvent::End => {
                        info!(reason = ?state.playing, position_secs = state.position.as_secs(), "ended");
                    }
                    TrackEvent::Error => {
                        if let PlayMode::Errored(error) = &state.playing {
                            warn!(%error, "track failed; queue will advance");
                        }
                    }
                    TrackEvent::Playable => debug!("upcoming track preloaded"),
                    _ => {}
                }
            }
        }
        None
    }
}
