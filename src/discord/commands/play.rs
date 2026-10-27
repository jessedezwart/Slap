//! Importing a link into the queue.

use std::{fmt::Write as _, sync::Arc};

use rand::seq::SliceRandom;
use tracing::debug;
use twilight_model::application::interaction::Interaction;

use super::{CommandError, Reply, caller_channel};
use crate::{
    audio::source::TrackSource,
    media::{PLAYLIST_LIMIT, validate_url},
    session::{Playlist, QueueEntry, QueueMode, SessionError},
    state::State,
};

pub(super) async fn play(
    state: &Arc<State>,
    interaction: &Interaction,
    raw_url: &str,
    mode: QueueMode,
) -> Result<Reply, CommandError> {
    let url = validate_url(raw_url)?;

    let (guild_id, channel_id) = caller_channel(state, interaction)?;
    let user_id = interaction.author_id().ok_or(CommandError::NotRanFromGuild)?;

    let cancel = state.sessions.join(guild_id, channel_id).await?;

    // Cover a caller leaving during the join handshake.
    crate::discord::events::reconcile_voice(Arc::clone(state), guild_id);

    let mut import = tokio::select! {
        () = cancel.cancelled() => return Err(SessionError::Ended.into()),
        result = state.importer.import(&url) => result?,
    };
    if import.tracks.is_empty() {
        return Ok(Reply::message(
            "That playlist contains no accessible tracks.",
        ));
    }

    // Imports are always shuffled. Not to be confused with the queue mode.
    import.tracks.shuffle(&mut rand::rng());
    let count = import.tracks.len();
    let playlist = import
        .playlist_name
        .take()
        .map(|name| {
            Arc::new(Playlist {
                name,
                url: url.clone(),
            })
        });
    let mut notice = String::new();
    if import.skipped > 0 {
        let _ = write!(notice, " Skipped {} unavailable entries.", import.skipped);
    }
    if import.truncated {
        let _ = write!(
            notice,
            " Import limited to the first {PLAYLIST_LIMIT} playlist entries."
        );
    }
    let entry_notice: Option<Arc<str>> = Some(notice.trim())
        .filter(|notice| !notice.is_empty())
        .map(Arc::from);
    let tracks = import
        .tracks
        .into_iter()
        .map(|track| {
            // The source plays the audio; the queue entry is what gets shown.
            let source = TrackSource::new(
                track.target,
                track.stream,
                Arc::clone(&state.audio),
                cancel.clone(),
            );
            let entry = QueueEntry {
                metadata: track.metadata,
                requester: user_id,
                playlist: playlist.clone(),
                notice: entry_notice.clone(),
            };
            (source.into(), entry)
        })
        .collect();
    let position = state
        .sessions
        .enqueue(guild_id, &cancel, tracks, mode)
        .await?;
    debug!(%guild_id, %channel_id, position, count, ?mode, "queued tracks");
    Ok(Reply {
        message: "Player loading...".into(),
        player: Some(cancel),
    })
}
