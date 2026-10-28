//! The task that keeps a player message current until its session ends.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use tokio::time;
use tracing::warn;
use twilight_model::{
    channel::{Message, message::MessageFlags},
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, MessageMarker},
    },
};

use super::view::{UP_NEXT_LEN, View};
use crate::{
    session::{GuildSession, QueueEntry, QueueHead},
    state::State,
};

/// How long the task gets to update the message.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the driver gets to report a track's state.
const INFO_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) struct PlayerTask {
    state: Arc<State>,
    guild_id: Id<GuildMarker>,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    session: GuildSession,
}

/// Why the task stopped updating the message.
enum Stop {
    /// The session ended: leave the message saying so.
    Ended,
    /// The message is gone or out of reach: nothing is left to update.
    Gone,
}

impl PlayerTask {
    pub fn new(
        state: Arc<State>,
        guild_id: Id<GuildMarker>,
        message: &Message,
        session: GuildSession,
    ) -> Self {
        Self {
            state,
            guild_id,
            channel_id: message.channel_id,
            message_id: message.id,
            session,
        }
    }

    pub async fn run(self) {
        match self.follow().await {
            Stop::Ended => self.stopped().await,
            // Releasing if gone is necessary because the session is still active, but the message is not.
            Stop::Gone => {
                self.state
                    .sessions
                    .release_player_component(self.guild_id, self.message_id)
                    .await;
            }
        }
    }

    /// Redraw on every change until the session ends or the message is gone.
    async fn follow(&self) -> Stop {
        loop {
            if let Err(stop) = self.redraw().await {
                return stop;
            }
            tokio::select! {
                biased;
                () = self.session.cancel.cancelled() => return Stop::Ended,
                () = self.session.redraw.notified() => {},
            }
        }
    }

    async fn redraw(&self) -> Result<(), Stop> {
        let Some(view) = self.render().await? else {
            return Ok(());
        };
        if self.session.cancel.is_cancelled() {
            return Err(Stop::Ended);
        }
        match edit(&self.state, self.channel_id, self.message_id, &view).await {
            Err(EditError::Gone) => Err(Stop::Gone),
            // A failed edit is not fatal.
            Ok(()) | Err(EditError::Failed) => Ok(()),
        }
    }

    async fn render(&self) -> Result<Option<View>, Stop> {
        let cancel = &self.session.cancel;
        let head = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Stop::Ended),
            head = self.state.sessions.now_playing(self.guild_id, UP_NEXT_LEN, cancel) => head,
        };
        let QueueHead { tracks, total } = head.ok_or(Stop::Ended)?;
        let mut tracks = tracks.into_iter();
        let Some(track) = tracks.next() else {
            return Ok(Some(View::empty()));
        };

        let info = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Stop::Ended),
            info = time::timeout(INFO_TIMEOUT, track.get_info()) => info,
        };

        let info = match info {
            Ok(Ok(info)) => info,
            Ok(Err(error)) => {
                warn!(%error, "track info unavailable");
                return Ok(None);
            }
            Err(_) => {
                warn!("track info timed out");
                return Ok(None);
            }
        };

        let next: Vec<_> = tracks.map(|track| track.data::<QueueEntry>()).collect();

        Ok(Some(View::playing(
            &track.data::<QueueEntry>(),
            &info,
            &next,
            total.saturating_sub(1),
            self.session.channel_id,
            SystemTime::now(),
        )))
    }

    async fn stopped(&self) {
        // Try to edit the player to say the session ended.
        let result = edit(
            &self.state,
            self.channel_id,
            self.message_id,
            &View::stopped(),
        )
        .await;
        // If can't edit, delete the message.
        if let Err(EditError::Failed) = result {
            delete_player(&self.state, self.channel_id, self.message_id).await;
        }
    }
}

pub(super) async fn delete_player(
    state: &State,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
) {
    let request = state.http.delete_message(channel_id, message_id);
    match time::timeout(HTTP_TIMEOUT, request).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!(%message_id, %error, "failed to delete player message"),
        Err(_) => warn!(%message_id, "player message delete timed out"),
    }
}

enum EditError {
    Gone,
    Failed,
}

async fn edit(
    state: &State,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    view: &View,
) -> Result<(), EditError> {
    // The first edit converts the plain /play reply into a V2 message, which requires dropping its text content.
    let request = state
        .http
        .update_message(channel_id, message_id)
        .content(None)
        .flags(MessageFlags::IS_COMPONENTS_V2)
        .components(Some(&view.components));
    match time::timeout(HTTP_TIMEOUT, request).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => {
            if matches!(error.kind(), twilight_http::error::ErrorType::Response { status, .. }
                if matches!(status.get(), 403 | 404))
            {
                warn!(%message_id, %error, "player message is no longer accessible");
                return Err(EditError::Gone);
            }
            warn!(%message_id, %error, "failed to update player message");
            Err(EditError::Failed)
        }
        Err(_) => {
            warn!(%message_id, "player message update timed out");
            Err(EditError::Failed)
        }
    }
}
