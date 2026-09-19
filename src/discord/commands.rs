//! Playback and queue slash commands.

use std::{fmt::Write as _, sync::Arc, time::Duration};

use rand::seq::SliceRandom;
use songbird::tracks::TrackHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{Interaction, InteractionData, application_command::CommandOptionValue},
    },
    channel::{
        Message,
        message::{AllowedMentions, MessageFlags},
    },
    http::interaction::{InteractionResponse, InteractionResponseType},
};
use twilight_util::builder::{
    InteractionResponseDataBuilder,
    command::{BooleanBuilder, CommandBuilder, IntegerBuilder, StringBuilder},
};

use crate::{
    audio::{
        source::OpusYtSource,
        ytdlp::{PLAYLIST_LIMIT, TrackMetadata, YtDlp},
    },
    discord::{player, state::State},
    error::AudioError,
    voice::session::{ControlResult, SessionError},
};

/// The command definition registered with Discord at start-up.
pub fn play_command() -> Command {
    CommandBuilder::new(
        "play",
        "Play a track or add a playlist to your voice channel",
        CommandType::ChatInput,
    )
    .option(
        StringBuilder::new("url", "An HTTP(S) track or playlist link")
            .required(true)
            .build(),
    )
    .option(
        BooleanBuilder::new("shuffle", "Shuffle the imported playlist before adding it").build(),
    )
    .build()
}

pub fn all_commands() -> Vec<Command> {
    let mut commands = vec![play_command()];
    for (name, description) in [
        ("pause", "Pause the current track"),
        ("resume", "Resume the paused track"),
        (
            "skip",
            "Skip the current track and play the next queued track",
        ),
        ("stop", "Stop playback, clear the queue, and disconnect"),
        (
            "shuffle",
            "Shuffle upcoming tracks without interrupting the current track",
        ),
    ] {
        commands.push(CommandBuilder::new(name, description, CommandType::ChatInput).build());
    }
    commands.push(
        CommandBuilder::new(
            "queue",
            "Show the current track and upcoming queue",
            CommandType::ChatInput,
        )
        .option(
            IntegerBuilder::new("page", "Queue page (five upcoming tracks per page)")
                .min_value(1)
                .build(),
        )
        .build(),
    );
    commands
}

/// Handle one registered playback interaction from start to finish.
pub async fn handle_interaction(state: Arc<State>, interaction: Interaction) {
    if let Some(InteractionData::MessageComponent(data)) = &interaction.data {
        if let Some(command) = player::button_command(&data.custom_id) {
            if let Err(error) = defer(&state, &interaction, true).await {
                warn!(%error, "failed to acknowledge player button");
                return;
            }
            let message = control(&state, &interaction, command, 1)
                .await
                .unwrap_or_else(|error| user_message(&error));
            if let Err(error) = follow_up(&state, &interaction, &message).await {
                warn!(%error, "failed to answer player button");
            }
        }
        return;
    }
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return;
    };

    if !matches!(
        data.name.as_str(),
        "play" | "pause" | "resume" | "skip" | "stop" | "shuffle" | "queue"
    ) {
        return;
    }

    // Resolving metadata takes a second or two, well past Discord's 3s limit
    // for a first response, so acknowledge before doing any work.
    if let Err(error) = defer(&state, &interaction, false).await {
        warn!(%error, "failed to acknowledge interaction");

        return;
    }

    let url = data
        .options
        .iter()
        .find(|o| o.name == "url")
        .and_then(|o| match &o.value {
            CommandOptionValue::String(value) => Some(value.as_str()),
            _ => None,
        });

    let shuffle = data
        .options
        .iter()
        .any(|o| o.name == "shuffle" && matches!(o.value, CommandOptionValue::Boolean(true)));
    let page = data
        .options
        .iter()
        .find_map(|o| match o.value {
            CommandOptionValue::Integer(page) if o.name == "page" => usize::try_from(page).ok(),
            _ => None,
        })
        .unwrap_or(1)
        .max(1);
    let result = if data.name == "play" {
        match url {
            Some(url) => play(&state, &interaction, url, shuffle).await,
            None => Ok(PlayReply::message(
                "You need to give me an HTTP(S) media link.",
            )),
        }
    } else {
        control(&state, &interaction, &data.name, page)
            .await
            .map(PlayReply::message)
    };
    let reply = result.unwrap_or_else(|error| PlayReply::message(user_message(&error)));

    match follow_up(&state, &interaction, &reply.message).await {
        Ok(response) => {
            if let Some((cancel, notice)) = reply.player {
                match response.model().await {
                    Ok(message) => {
                        if let Some(guild_id) = interaction.guild_id {
                            player::start(Arc::clone(&state), guild_id, &cancel, message, notice)
                                .await;
                        }
                    }
                    Err(error) => warn!(%error, "failed to read player message"),
                }
            }
        }
        Err(error) => warn!(%error, "failed to send interaction follow-up"),
    }
}

struct PlayReply {
    message: String,
    player: Option<(CancellationToken, String)>,
}

impl PlayReply {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            player: None,
        }
    }
}

/// Everything that can go wrong in a way the user should hear about.
#[derive(Debug, thiserror::Error)]
enum PlayError {
    #[error("guild only")]
    NotInGuild,

    #[error("caller is not in a voice channel")]
    NotInVoice,

    #[error(transparent)]
    Audio(#[from] AudioError),

    #[error(transparent)]
    Session(#[from] SessionError),
}

async fn play(
    state: &Arc<State>,
    interaction: &Interaction,
    raw_url: &str,
    shuffle: bool,
) -> Result<PlayReply, PlayError> {
    let guild_id = interaction.guild_id.ok_or(PlayError::NotInGuild)?;
    let user_id = interaction.author_id().ok_or(PlayError::NotInGuild)?;

    // Reject the link before spawning anything.
    let url = YtDlp::validate_url(raw_url)?;

    let channel_id = state
        .cache
        .voice_state(user_id, guild_id)
        .map(|voice_state| voice_state.channel_id())
        .ok_or(PlayError::NotInVoice)?;

    let cancel = state.sessions.join(guild_id, channel_id).await?;

    // Also covers a caller leaving during the join handshake.
    crate::discord::events::reconcile_voice(Arc::clone(state), guild_id);
    let list = async {
        if crate::audio::spotify::is_spotify_url(&url) {
            state.spotify.list(&url).await
        } else {
            state.ytdlp.list(&url).await
        }
    };
    let mut listing = tokio::select! {
        () = cancel.cancelled() => return Err(AudioError::Cancelled.into()),
        result = list => result?,
    };
    if listing.tracks.is_empty() {
        return Ok(PlayReply::message(
            "That playlist contains no accessible tracks.",
        ));
    }
    if shuffle {
        listing.tracks.shuffle(&mut rand::rng());
    }
    let count = listing.tracks.len();
    let title = listing.tracks[0]
        .metadata
        .title
        .clone()
        .unwrap_or_else(|| raw_url.to_owned());
    let duration = listing.tracks[0].metadata.duration;
    let tracks = listing
        .tracks
        .into_iter()
        .map(|entry| {
            let mut source = OpusYtSource::new(
                entry.target,
                state.client.clone(),
                state.ytdlp.clone(),
                state.config.buffer,
                cancel.clone(),
            );
            if let Some(resolved) = listing.single.take() {
                source = source.with_resolved(resolved);
            }
            (source.into(), entry.metadata)
        })
        .collect();
    let position = state.sessions.enqueue(guild_id, &cancel, tracks).await?;
    info!(%guild_id, %channel_id, position, count, "queued tracks");
    let mut message = if count == 1 {
        let mut message = if position == 1 {
            format!("Now playing **{}**", escape(&title))
        } else {
            format!("Queued **{}** at position {}", escape(&title), position - 1)
        };
        if let Some(duration) = duration {
            let _ = write!(message, " ({})", format_duration(duration));
        }
        message
    } else {
        format!(
            "Added {count} tracks{} to the queue.{}",
            if shuffle { " in shuffled order" } else { "" },
            if position == 1 {
                " Playback is starting."
            } else {
                ""
            }
        )
    };
    let mut notice = String::new();
    if listing.skipped > 0 {
        let _ = write!(notice, " Skipped {} unavailable entries.", listing.skipped);
    }
    if listing.truncated {
        let _ = write!(
            notice,
            " Import limited to the first {PLAYLIST_LIMIT} playlist entries."
        );
    }
    message.push_str(&notice);
    Ok(PlayReply {
        message,
        player: Some((cancel, notice)),
    })
}

async fn control(
    state: &Arc<State>,
    interaction: &Interaction,
    command: &str,
    page: usize,
) -> Result<String, PlayError> {
    let guild_id = interaction.guild_id.ok_or(PlayError::NotInGuild)?;
    let user_id = interaction.author_id().ok_or(PlayError::NotInGuild)?;
    let channel_id = state
        .cache
        .voice_state(user_id, guild_id)
        .map(|voice| voice.channel_id())
        .ok_or(PlayError::NotInVoice)?;
    let panel_message = if matches!(interaction.data, Some(InteractionData::MessageComponent(_))) {
        Some(
            interaction
                .message
                .as_ref()
                .ok_or(SessionError::StalePanel)?
                .id,
        )
    } else {
        None
    };
    match state
        .sessions
        .control(guild_id, channel_id, command, page, panel_message)
        .await?
    {
        ControlResult::Message(message) => Ok(message),
        ControlResult::Queue {
            tracks,
            total,
            page,
        } => {
            let metadata: Vec<_> = tracks
                .iter()
                .map(TrackHandle::data::<TrackMetadata>)
                .collect();
            Ok(format_queue(&metadata, total, page))
        }
    }
}

fn format_queue(tracks: &[Arc<TrackMetadata>], total: usize, page: usize) -> String {
    let Some(current) = tracks.first() else {
        return "The queue is empty.".into();
    };
    let pages = total.saturating_sub(1).div_ceil(5).max(1);
    if page == 0 || page > pages {
        return format!("That page does not exist. Choose a page from 1 to {pages}.");
    }
    let label = |metadata: &TrackMetadata| {
        let title: String = metadata
            .title
            .as_deref()
            .or(metadata.webpage_url.as_deref())
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
        "**Current track:** {}\n**Queue:** {} upcoming • page {page}/{pages}",
        label(current),
        total.saturating_sub(1)
    );
    for (offset, track) in tracks.iter().skip(1).take(5).enumerate() {
        let index = 1 + (page - 1) * 5 + offset;
        let _ = write!(message, "\n{index}. {}", label(track));
    }
    if pages > 1 {
        message.push_str("\nUse `/queue page:<number>` to see another page.");
    }
    message
}

fn user_message(error: &PlayError) -> String {
    match error {
        PlayError::NotInGuild => "This command only works inside a server.".to_owned(),
        PlayError::NotInVoice => "Join a voice channel first.".to_owned(),
        PlayError::Session(error) => error.to_string(),
        PlayError::Audio(AudioError::Cancelled) => {
            "Playback was stopped or the voice session ended.".into()
        }
        PlayError::Audio(AudioError::UnsupportedUrl(_)) => {
            "That does not look like a valid HTTP(S) link.".to_owned()
        }
        PlayError::Audio(AudioError::NoOpusFormat) => {
            "That video has no Opus audio track, so I cannot stream it.".to_owned()
        }
        PlayError::Audio(AudioError::Spawn { program, .. }) => {
            format!("I could not run `{program}`. Is yt-dlp installed?")
        }
        PlayError::Audio(AudioError::Timeout(_)) => {
            "yt-dlp took too long to answer. Try again.".to_owned()
        }
        other => {
            warn!(error = %other, "play command failed");

            format!("Something went wrong: {other}")
        }
    }
}

async fn defer(
    state: &Arc<State>,
    interaction: &Interaction,
    private: bool,
) -> Result<(), twilight_http::Error> {
    state
        .http
        .interaction(state.application_id)
        .create_response(
            interaction.id,
            &interaction.token,
            &InteractionResponse {
                kind: InteractionResponseType::DeferredChannelMessageWithSource,
                data: private.then(|| {
                    InteractionResponseDataBuilder::new()
                        .flags(MessageFlags::EPHEMERAL)
                        .build()
                }),
            },
        )
        .await
        .map(|_| ())
}

async fn follow_up(
    state: &Arc<State>,
    interaction: &Interaction,
    message: &str,
) -> Result<twilight_http::Response<Message>, twilight_http::Error> {
    state
        .http
        .interaction(state.application_id)
        .update_response(&interaction.token)
        .content(Some(message))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await
}

/// Stop a track title from breaking out of its bold markers or pinging anyone.
pub(super) fn escape(text: &str) -> String {
    let trimmed: String = text.chars().take(200).collect();

    trimmed
        .replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('`', "\\`")
        .replace('~', "\\~")
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
        .replace('@', "@\u{200b}")
}

pub(super) fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Follow-ups are visible to the channel; errors stay between us and the user.
#[allow(dead_code)]
fn ephemeral(message: &str) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(
            InteractionResponseDataBuilder::new()
                .content(message)
                .flags(MessageFlags::EPHEMERAL)
                .build(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_pages_are_bounded_and_keep_positions() {
        let tracks: Vec<_> = (0..13)
            .map(|i| {
                Arc::new(TrackMetadata {
                    title: Some(format!("Song {i}")),
                    ..TrackMetadata::default()
                })
            })
            .collect();
        let visible = std::iter::once(tracks[0].clone())
            .chain(tracks[6..11].iter().cloned())
            .collect::<Vec<_>>();
        let page = format_queue(&visible, tracks.len(), 2);
        assert!(page.contains("Current track:** Song 0"));
        assert!(page.contains("page 2/3"));
        assert!(page.contains("6. Song 6"));
        assert!(page.contains("10. Song 10"));
        assert!(!page.contains("11. Song 11"));
        assert!(format_queue(&tracks, tracks.len(), usize::MAX).contains("does not exist"));
        assert_eq!(format_queue(&[], 0, 1), "The queue is empty.");
        let metadata = Arc::new(TrackMetadata {
            title: Some("*@\n".repeat(100)),
            ..TrackMetadata::default()
        });
        let long = vec![metadata; 10];
        let message = format_queue(&long, long.len(), 1);
        assert!(message.chars().count() < 2000);
        assert_eq!(message.lines().count(), 8);
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_secs(5)), "0:05");
        assert_eq!(format_duration(Duration::from_secs(212)), "3:32");
        assert_eq!(format_duration(Duration::from_secs(3_725)), "1:02:05");
    }

    #[test]
    fn escapes_markdown_and_mentions() {
        assert_eq!(escape("a*b_c`d"), "a\\*b\\_c\\`d");
        assert!(escape("@everyone").starts_with("@\u{200b}"));
    }

    #[test]
    fn truncates_long_titles() {
        let long = "x".repeat(500);

        assert_eq!(escape(&long).chars().count(), 200);
    }
}
