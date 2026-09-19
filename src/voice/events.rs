//! songbird event handlers.
//!
//! These run on songbird's driver task, so they must never reach back into the
//! `Call` they are attached to — that would re-enter a lock songbird is already
//! holding. They only post a message and return.

use songbird::{
    events::{Event, EventContext, EventHandler},
    tracks::PlayMode,
};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use twilight_model::id::{Id, marker::GuildMarker};

/// Work that must happen off the driver task.
#[derive(Debug)]
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

/// Logs track failures. The builtin queue advances past them on its own.
pub struct TrackErrorNotifier;

/// Coalesced wakeup only: no HTTP, queue scans or locks on the driver task.
pub struct PlayerNotifier(pub Arc<Notify>);

#[async_trait::async_trait]
impl EventHandler for PlayerNotifier {
    async fn act(&self, _: &EventContext<'_>) -> Option<Event> {
        self.0.notify_one();
        None
    }
}

#[async_trait::async_trait]
impl EventHandler for TrackErrorNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(states) = ctx {
            for (state, handle) in *states {
                if let PlayMode::Errored(error) = &state.playing {
                    warn!(track = ?handle.uuid(), %error, "track failed");
                }
            }
        }

        None
    }
}
