//! Importing a link into the queue.

use std::{fmt::Write as _, sync::Arc};

use rand::seq::SliceRandom;
use twilight_model::application::interaction::Interaction;

use super::{CommandError, Reply, caller_channel};
use crate::{
    audio::source::TrackSource,
    media::{PLAYLIST_LIMIT, validate_url},
    session::{Origin, QueueEntry, QueueMode, SessionError},
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
    // Server nickname, else display name, else username.
    let user_name = interaction
        .member
        .as_ref()
        .and_then(|member| member.nick.clone())
        .or_else(|| {
            let user = interaction.author()?;
            Some(user.global_name.clone().unwrap_or_else(|| user.name.clone()))
        })
        .unwrap_or_default();

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

    let origin = Arc::new(Origin {
        requester: user_id,
        requester_name: user_name,
        url,
        playlist_name: import.playlist_name.take(),
        skipped: import.skipped,
        truncated: import.truncated,
    });
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
                origin: Arc::clone(&origin),
            };
            (source.into(), entry)
        })
        .collect();
    state
        .sessions
        .enqueue(guild_id, &cancel, tracks, mode)
        .await?;
    let mut message = if count == 1 {
        "Queued 1 track.".to_owned()
    } else {
        format!("Queued {count} tracks.")
    };
    if origin.skipped > 0 {
        let _ = write!(message, " Skipped {} unavailable entries.", origin.skipped);
    }
    if origin.truncated {
        let _ = write!(
            message,
            " Import limited to the first {PLAYLIST_LIMIT} playlist entries."
        );
    }
    Ok(Reply {
        message,
        player: Some(cancel),
    })
}
