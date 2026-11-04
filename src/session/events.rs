//! songbird event handlers.

use std::sync::{Arc, Weak};

use songbird::{
    Call,
    events::{CoreEvent, Event, EventContext, EventHandler, TrackEvent},
    tracks::{PlayMode, TrackQueue},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use twilight_model::id::{Id, marker::GuildMarker};

use super::{PlayTally, QueueEntry, SessionManager, queue::prefetch_next};

/// Replace the call's global events with this session's handlers.
pub fn attach(
    call: &mut Call,
    guild_id: Id<GuildMarker>,
    sessions: Weak<SessionManager>,
    cancel: &CancellationToken,
    redraw: &Arc<Notify>,
    played: &Arc<PlayTally>,
) {
    // Events are set explicitly
    call.remove_all_global_events();
    // Handle disconnect
    call.add_global_event(
        CoreEvent::DriverDisconnect.into(),
        DisconnectHandler {
            guild_id,
            sessions,
            cancel: cancel.clone(),
        },
    );
    // Handle track events
    for event in [
        TrackEvent::Play,
        TrackEvent::Pause,
        TrackEvent::End,
        TrackEvent::Error,
        TrackEvent::Playable,
    ] {
        call.add_global_event(
            event.into(),
            TrackEventHandler {
                guild_id,
                redraw: Arc::clone(redraw),
            },
        );
    }
    // Tally every track as it ends; errored tracks fire End too
    call.add_global_event(
        TrackEvent::End.into(),
        TallyPlayed {
            played: Arc::clone(played),
        },
    );
    // Look up the next track whenever one starts playing
    let queue = call.queue().clone();
    call.add_global_event(TrackEvent::Play.into(), PrefetchNext { queue });
}

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

/// Adds each ended track to the session's tally.
pub struct TallyPlayed {
    pub played: Arc<PlayTally>,
}

#[async_trait::async_trait]
impl EventHandler for TallyPlayed {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(states) = ctx {
            for (state, handle) in *states {
                self.played
                    .record(state.play_time, &handle.data::<QueueEntry>());
            }
        }
        None
    }
}

/// Starts looking up the next track as soon as the one before it plays.
pub struct PrefetchNext {
    pub queue: TrackQueue,
}

#[async_trait::async_trait]
impl EventHandler for PrefetchNext {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        prefetch_next(&self.queue);
        None
    }
}
