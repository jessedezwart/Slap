//! Which player component owns a session, and what it can do with it.

use songbird::tracks::TrackQueue;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker},
};

use super::{GuildSession, SessionError, SessionManager};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Skip,
    /// Clear the queue and leave the channel.
    Stop,
}

impl SessionManager {
    /// Handles interactions
    pub async fn control(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        player_component: Id<MessageMarker>,
        control: Control,
    ) -> Result<(), SessionError> {
        let mut session = self.lock(guild_id).await;
        check_player(&session, channel_id, player_component)?;
        let apply: fn(&TrackQueue) -> _ = match control {
            Control::Stop => {
                self.teardown_locked(guild_id, &mut session, "stop command")
                    .await;
                return Ok(());
            }
            Control::Pause => TrackQueue::pause,
            Control::Resume => TrackQueue::resume,
            Control::Skip => TrackQueue::skip,
        };
        let call = self.call(guild_id)?;
        let call = call.lock().await;
        let queue = call.queue();
        if queue.is_empty() {
            return Err(SessionError::EmptyQueue);
        }
        apply(queue).map_err(|_| SessionError::TrackFinished)?;
        debug!(%guild_id, %channel_id, ?control, remaining = queue.len(), "playback control applied");
        Ok(())
    }

    /// Link player component to session
    pub async fn claim_player_component(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        player_component_id: Id<MessageMarker>,
    ) -> Option<GuildSession> {
        let mut session = self.lock(guild_id).await;
        let session = session
            .as_mut()
            .filter(|session| !cancel.is_cancelled() && session.player_component_id.is_none())?;
        session.player_component_id = Some(player_component_id);
        Some(session.clone())
    }

    pub async fn release_player_component(
        &self,
        guild_id: Id<GuildMarker>,
        message_id: Id<MessageMarker>,
    ) {
        let mut session = self.lock(guild_id).await;
        if let Some(session) = session
            .as_mut()
            .filter(|session| session.player_component_id == Some(message_id))
        {
            session.player_component_id = None;
        }
    }
}

/// Check whether the player component is on the current session and linked to the current channel.
fn check_player(
    session: &Option<GuildSession>,
    channel_id: Id<ChannelMarker>,
    player_component: Id<MessageMarker>,
) -> Result<(), SessionError> {
    let session = session
        .as_ref()
        .filter(|session| session.player_component_id == Some(player_component))
        .ok_or(SessionError::Ended)?;
    if session.channel_id != channel_id {
        return Err(SessionError::WrongChannel);
    }
    Ok(())
}
