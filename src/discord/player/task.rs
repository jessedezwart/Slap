//! The task that keeps a player message current until its session ends.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use tokio::time;
use tracing::warn;
use twilight_model::{
    channel::{
        Message,
        message::{AllowedMentions, MessageFlags},
    },
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, MessageMarker},
    },
};

use super::view::View;
use crate::{
    discord::state::State,
    voice::{
        queue::QueueEntry,
        session::{GuildSession, PlayerSnapshot},
    },
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
    /// Shown under every view: skipped entries, truncated imports.
    notice: String,
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
        notice: String,
    ) -> Self {
        Self {
            state,
            guild_id,
            channel_id: message.channel_id,
            message_id: message.id,
            session,
            notice,
        }
    }

    pub async fn run(self) {
        loop {
            match self.redraw().await {
                Ok(()) => {}
                Err(Stop::Ended) => break,
                Err(Stop::Gone) => return,
            }
            // Notify stores a single permit, so a burst of events while an
            // edit is in flight costs one more redraw, not one per event.
            tokio::select! {
                biased;
                () = self.session.cancel.cancelled() => break,
                () = self.session.updates.notified() => {},
            }
        }
        self.stopped().await;
    }

    /// Rewrite the message from the current state.
    async fn redraw(&self) -> Result<(), Stop> {
        let Some(view) = self.render().await? else {
            return Ok(());
        };
        if self.session.cancel.is_cancelled() {
            return Err(Stop::Ended);
        }
        match edit(&self.state, self.channel_id, self.message_id, &view).await {
            Err(EditError::Gone) => Err(Stop::Gone),
            Ok(()) | Err(EditError::Failed) => Ok(()),
        }
    }

    async fn render(&self) -> Result<Option<View>, Stop> {
        let cancel = &self.session.cancel;
        let snapshot = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Stop::Ended),
            snapshot = self.state.sessions.player_snapshot(self.guild_id, cancel) => snapshot,
        };
        let PlayerSnapshot { track, upcoming } = snapshot.ok_or(Stop::Ended)?;
        let Some(track) = track else {
            return Ok(Some(View::empty(&self.notice)));
        };
        // get_info is a driver message/oneshot, not an HTTP request. Never
        // wait for it while holding a session, queue, or Call lock.
        let info = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Stop::Ended),
            info = time::timeout(INFO_TIMEOUT, track.get_info()) => info,
        };
        let Ok(Ok(info)) = info else {
            return Ok(None);
        };
        Ok(Some(View::playing(
            &track.data::<QueueEntry>(),
            &info,
            upcoming,
            &self.notice,
            self.session.channel_id,
            SystemTime::now(),
        )))
    }

    async fn stopped(&self) {
        if edit(
            &self.state,
            self.channel_id,
            self.message_id,
            &View::stopped(),
        )
        .await
        .is_err()
        {
            delete_player(&self.state, self.channel_id, self.message_id).await;
        }
    }
}

pub(super) async fn delete_player(
    state: &State,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
) {
    match time::timeout(
        HTTP_TIMEOUT,
        state.http.delete_message(channel_id, message_id),
    )
    .await
    {
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
    let mentions = AllowedMentions::default();
    // The first edit converts the plain /play reply into a V2 message, which requires dropping its text content.
    let request = state
        .http
        .update_message(channel_id, message_id)
        .content(None)
        .flags(MessageFlags::IS_COMPONENTS_V2)
        .components(Some(&view.components))
        .allowed_mentions(Some(&mentions));
    match time::timeout(HTTP_TIMEOUT, request).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => {
            // Deleted message / lost access: stop updating. A later /play can
            // claim a new panel. Use the bot token so edits work after 15 min.
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
