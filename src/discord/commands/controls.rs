//! The player's playback controls and queue listing.

use std::{fmt::Write as _, sync::Arc};

use songbird::tracks::TrackHandle;
use tokio_util::sync::CancellationToken;
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    id::{Id, marker::GuildMarker},
};

use super::{PlayError, caller_channel};
use crate::{
    discord::{
        state::State,
        text::{escape, format_duration},
    },
    media::Metadata,
    voice::{
        queue::QueueEntry,
        session::{Control, QueueHead},
    },
};

/// Upcoming tracks listed by the queue command.
const QUEUE_LEN: usize = 10;

/// The voice session a player button belongs to
fn panel_session(
    state: &State,
    interaction: &Interaction,
    guild_id: Id<GuildMarker>,
) -> Result<Option<CancellationToken>, PlayError> {
    if !matches!(interaction.data, Some(InteractionData::MessageComponent(_))) {
        return Ok(None);
    }
    let message = interaction.message.as_ref().ok_or(PlayError::StalePanel)?;
    state
        .panels
        .session(guild_id, message.id)
        .map(Some)
        .ok_or(PlayError::StalePanel)
}

pub(super) async fn control_playback(
    state: &Arc<State>,
    interaction: &Interaction,
    control: Control,
) -> Result<String, PlayError> {
    let (guild_id, channel_id) = caller_channel(state, interaction)?;
    let session = panel_session(state, interaction, guild_id)?;
    state
        .sessions
        .control(guild_id, channel_id, session.as_ref(), control)
        .await?;
    Ok(match control {
        Control::Pause => "Paused. Press Resume to continue.",
        Control::Resume => "Resumed playback.",
        Control::Skip => "Skipped the current track.",
        Control::Stop => "Stopped playback, cleared the queue, and disconnected.",
    }
    .to_owned())
}

pub(super) async fn queue(
    state: &Arc<State>,
    interaction: &Interaction,
) -> Result<String, PlayError> {
    let (guild_id, channel_id) = caller_channel(state, interaction)?;
    let session = panel_session(state, interaction, guild_id)?;
    let QueueHead { tracks, total } = state
        .sessions
        .queue_head(guild_id, channel_id, session.as_ref(), QUEUE_LEN)
        .await?;
    let entries: Vec<_> = tracks.iter().map(TrackHandle::data::<QueueEntry>).collect();
    let metadata: Vec<_> = entries.iter().map(|entry| &entry.metadata).collect();
    Ok(format_queue(&metadata, total))
}

fn format_queue(tracks: &[&Metadata], total: usize) -> String {
    let Some(current) = tracks.first() else {
        return "The queue is empty.".into();
    };
    let label = |metadata: &Metadata| {
        let title: String = metadata
            .title
            .as_deref()
            .or(metadata.webpage_url.as_ref().map(url::Url::as_str))
            .unwrap_or("Unknown track")
            .chars()
            .take(80)
            .collect();
        let mut label = escape(&title);
        if let Some(duration) = metadata.duration {
            let _ = write!(label, " ({})", format_duration(duration));
        }
        label
    };
    let mut message = format!(
        "**Current track:** {}
**Queue:** {} upcoming",
        label(current),
        total.saturating_sub(1)
    );
    for (index, track) in tracks.iter().skip(1).take(QUEUE_LEN).enumerate() {
        let _ = write!(message, "
{}. {}", index + 1, label(track));
    }
    message
}

#[cfg(test)]
#[path = "../../../tests/discord/commands/controls.rs"]
mod tests;
