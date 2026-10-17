//! songbird event handlers.
//!
//! These run on songbird's driver task, so they must never reach back into the
//! `Call` they are attached to — that would re-enter a lock songbird is already
//! holding. They post notifications and write bounded, non-blocking logs.

use crate::voice::queue::QueueEntry;
use songbird::{
    events::{Event, EventContext, EventHandler, TrackEvent},
    tracks::{PlayMode, ReadyState},
};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use twilight_model::id::{Id, marker::GuildMarker};

/// Work that must happen off the driver task.
pub enum VoiceCommand {
    Teardown {
        guild_id: Id<GuildMarker>,
        reason: String,
        cancel: CancellationToken,
    },
}

/// Turns any voice disconnect into a full teardown of the guild's pipeline.
pub struct DriverDisconnectNotifier {
    pub guild_id: Id<GuildMarker>,
    pub commands: UnboundedSender<VoiceCommand>,
    pub cancel: CancellationToken,
}

#[async_trait::async_trait]
impl EventHandler for DriverDisconnectNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::DriverDisconnect(data) = ctx {
            let reason = match data.reason {
                Some(reason) => format!("{:?} ({reason:?})", data.kind),
                None => format!("{:?}", data.kind),
            };

            let _ = self.commands.send(VoiceCommand::Teardown {
                guild_id: self.guild_id,
                reason,
                cancel: self.cancel.clone(),
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
    pub updates: Arc<Notify>,
}

#[async_trait::async_trait]
impl EventHandler for PlayerNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        self.updates.notify_one();
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
