//! Importing a link into the queue.

use std::{fmt::Write as _, sync::Arc, time::Instant};

use rand::seq::SliceRandom;
use tracing::debug;
use twilight_model::application::interaction::Interaction;

use super::{CommandError, NewPlayer, Reply, caller_channel};
use crate::{
    audio::source::TrackSource,
    discord::{
        state::State,
        text::{escape, format_duration, playlist_link},
    },
    media::{PLAYLIST_LIMIT, validate_url},
    voice::session::{Playlist, QueueEntry, QueueMode, SessionError},
};

pub(super) async fn play(
    state: &Arc<State>,
    interaction: &Interaction,
    raw_url: &str,
    mode: QueueMode,
) -> Result<Reply, CommandError> {
    // Reject the link before spawning anything.
    let url = validate_url(raw_url)?;

    let (guild_id, channel_id) = caller_channel(state, interaction)?;
    let user_id = interaction.author_id().ok_or(CommandError::NotInGuild)?;

    let cancel = state.sessions.join(guild_id, channel_id).await?;

    // Also covers a caller leaving during the join handshake.
    crate::discord::events::reconcile_voice(Arc::clone(state), guild_id);
    debug!(?mode, "loading track or playlist metadata");
    let started = Instant::now();
    let mut import = tokio::select! {
        () = cancel.cancelled() => return Err(SessionError::Ended.into()),
        result = state.importer.import(&url) => result?,
    };
    debug!(
        tracks = import.tracks.len(),
        skipped = import.skipped,
        truncated = import.truncated,
        elapsed_ms = started.elapsed().as_millis(),
        "media import completed"
    );
    if import.tracks.is_empty() {
        return Ok(Reply::message(
            "That playlist contains no accessible tracks.",
        ));
    }
    // Imports are always shuffled.
    import.tracks.shuffle(&mut rand::rng());
    let count = import.tracks.len();
    // One allocation for the whole import, shared by every track of it.
    let playlist = import
        .playlist_name
        .take()
        .filter(|_| count > 1)
        .map(|name| {
            Arc::new(Playlist {
                name,
                url: url.clone(),
            })
        });
    let title = import.tracks[0]
        .metadata
        .title
        .clone()
        .unwrap_or_else(|| raw_url.to_owned());
    let duration = import.tracks[0].metadata.duration;
    let tracks = import
        .tracks
        .into_iter()
        .map(|track| {
            // The source plays the audio; the queue entry is what gets shown.
            let source = TrackSource::new(
                track.target,
                track.stream,
                Arc::clone(&state.audio),
                guild_id,
                cancel.clone(),
            );
            let entry = QueueEntry {
                metadata: track.metadata,
                requester: user_id,
                playlist: playlist.clone(),
            };
            (source.into(), entry)
        })
        .collect();
    let position = state
        .sessions
        .enqueue(guild_id, &cancel, tracks, mode)
        .await?;
    debug!(%guild_id, %channel_id, position, count, ?mode, "queued tracks");
    // A replaced queue starts over, so those imports report position 1 too.
    let jumped = mode == QueueMode::Next && position > 1;
    let mut message = if count == 1 {
        let mut message = if position == 1 {
            format!("Now playing **{}**", escape(&title))
        } else if jumped {
            format!("Queued **{}** to play next", escape(&title))
        } else {
            format!("Queued **{}** at position {}", escape(&title), position - 1)
        };
        if let Some(duration) = duration {
            let _ = write!(message, " ({})", format_duration(duration));
        }
        message
    } else {
        format!(
            "Added {count} tracks{} in shuffled order{}.{}",
            match playlist.as_deref() {
                Some(playlist) => format!(" from {}", playlist_link(playlist)),
                None => String::new(),
            },
            if jumped {
                " to the front of the queue"
            } else {
                " to the queue"
            },
            if position == 1 {
                " Playback is starting."
            } else {
                ""
            }
        )
    };
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
    message.push_str(&notice);
    Ok(Reply {
        message,
        player: Some(NewPlayer { cancel, notice }),
    })
}
