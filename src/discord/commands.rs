//! Playback and queue slash commands.

use std::{
    fmt::Write as _,
    sync::Arc,
    time::{Duration, Instant},
};

use rand::seq::SliceRandom;
use songbird::tracks::TrackHandle;
use tokio_util::sync::CancellationToken;
use tracing::{Span, info, warn};
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{
            Interaction, InteractionData,
            application_command::{CommandDataOption, CommandOptionValue},
        },
    },
    channel::message::{AllowedMentions, MessageFlags},
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
        ("shuffle", "Turn shuffle on or off for upcoming tracks"),
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

/// Buttons and slash commands share authorization, execution, and logging.
#[tracing::instrument(skip_all, fields(
    guild_id = interaction.guild_id.map(|id| id.get()), user_id = interaction.author_id().map(|id| id.get()),
    interaction_id = %interaction.id, command = tracing::field::Empty,
    source = tracing::field::Empty,
))]
pub async fn handle_interaction(state: Arc<State>, interaction: Interaction) {
    let (command, options, button) = match &interaction.data {
        Some(InteractionData::ApplicationCommand(data))
            if matches!(
                data.name.as_str(),
                "play" | "pause" | "resume" | "skip" | "stop" | "shuffle" | "queue"
            ) =>
        {
            (data.name.as_str(), data.options.as_slice(), false)
        }
        Some(InteractionData::MessageComponent(data)) => {
            let Some(command) = player::button_command(&data.custom_id) else {
                return;
            };
            (command, &[][..], true)
        }
        _ => return,
    };
    Span::current().record("command", command);
    Span::current().record("source", if button { "button" } else { "slash" });
    let mode = ReplyMode::new(command, button);
    let started = Instant::now();
    info!("player interaction received");
    if let Err(error) = defer(&state, &interaction, mode).await {
        warn!(%error, "failed to acknowledge interaction");
        return;
    }
    let result = execute(&state, &interaction, command, options).await;
    match &result {
        Ok(_) => info!(
            elapsed_ms = started.elapsed().as_millis(),
            "player interaction completed"
        ),
        Err(error) => {
            warn!(%error, elapsed_ms = started.elapsed().as_millis(), "player interaction rejected or failed")
        }
    }
    if let Err(error) = respond(&state, &interaction, mode, result).await {
        warn!(%error, "failed to send interaction response");
    }
}

async fn execute(
    state: &Arc<State>,
    interaction: &Interaction,
    command: &str,
    options: &[CommandDataOption],
) -> Result<PlayReply, PlayError> {
    let mut url = None;
    let mut shuffle = false;
    let mut page = 1;
    for option in options {
        match (option.name.as_str(), &option.value) {
            ("url", CommandOptionValue::String(value)) => url = Some(value.as_str()),
            ("shuffle", CommandOptionValue::Boolean(value)) => shuffle = *value,
            ("page", CommandOptionValue::Integer(value)) => {
                page = usize::try_from(*value).unwrap_or(1).max(1)
            }
            _ => {}
        }
    }
    if command == "play" {
        match url {
            Some(url) => play(state, interaction, url, shuffle).await,
            None => Ok(PlayReply::message(
                "You need to give me an HTTP(S) media link.",
            )),
        }
    } else {
        control(state, interaction, command, page)
            .await
            .map(PlayReply::message)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ReplyMode {
    Public,
    Private,
    SilentButton,
    SilentCommand,
}

impl ReplyMode {
    fn new(command: &str, button: bool) -> Self {
        // The player reflects every panel action, so only the queue replies.
        let silent = if button {
            command != "queue"
        } else {
            matches!(command, "skip" | "stop" | "shuffle")
        };
        match (silent, button) {
            (true, true) => Self::SilentButton,
            (true, false) => Self::SilentCommand,
            (false, true) => Self::Private,
            (false, false) => Self::Public,
        }
    }
}

async fn respond(
    state: &Arc<State>,
    interaction: &Interaction,
    mode: ReplyMode,
    result: Result<PlayReply, PlayError>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let http = state.http.interaction(state.application_id);
    if result.is_ok() {
        match mode {
            ReplyMode::SilentButton => return Ok(()),
            ReplyMode::SilentCommand => {
                http.delete_response(&interaction.token).await?;
                return Ok(());
            }
            _ => {}
        }
    }
    let reply = result.unwrap_or_else(|error| PlayReply::message(user_message(&error)));
    let mentions = AllowedMentions::default();
    if mode == ReplyMode::SilentButton {
        // A deferred button update targets the player itself: errors must be
        // separate private follow-ups, never overwrite the live player.
        http.create_followup(&interaction.token)
            .content(&reply.message)
            .flags(MessageFlags::EPHEMERAL)
            .allowed_mentions(Some(&mentions))
            .await?;
    } else {
        let response = http
            .update_response(&interaction.token)
            .content(Some(&reply.message))
            .allowed_mentions(Some(&mentions))
            .await?;
        if let Some((cancel, notice)) = reply.player
            && let Some(guild_id) = interaction.guild_id
        {
            player::start(
                Arc::clone(state),
                guild_id,
                &cancel,
                response.model().await?,
                notice,
            )
            .await;
        }
    }
    Ok(())
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
    let provider = if crate::audio::spotify::is_spotify_url(&url) {
        "spotify"
    } else {
        "yt-dlp"
    };
    info!(
        provider,
        host = url.host_str(),
        shuffle,
        "loading track or playlist metadata"
    );
    let started = Instant::now();
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
    info!(
        tracks = listing.tracks.len(),
        skipped = listing.skipped,
        truncated = listing.truncated,
        elapsed_ms = started.elapsed().as_millis(),
        "media import completed"
    );
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
        .map(|mut entry| {
            entry.metadata.requester = Some(user_id);
            let mut source = OpusYtSource::new(
                entry.target,
                state.client.clone(),
                state.ytdlp.clone(),
                state.config.buffer,
                cancel.clone(),
            )
            .in_guild(guild_id);
            if let Some(resolved) = listing.single.take() {
                source = source.with_resolved(resolved);
            }
            (source.into(), entry.metadata)
        })
        .collect();
    let (position, shuffled) = state.sessions.enqueue(guild_id, &cancel, tracks).await?;
    info!(%guild_id, %channel_id, position, count, shuffled, "queued tracks");
    let mut message = if count == 1 {
        let mut message = if position == 1 {
            format!("Now playing **{}**", escape(&title))
        } else if shuffled {
            format!("Queued **{}** at a random position", escape(&title))
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
            if shuffle || shuffled {
                " in shuffled order"
            } else {
                ""
            },
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
        other => format!("Something went wrong: {other}"),
    }
}

async fn defer(
    state: &Arc<State>,
    interaction: &Interaction,
    mode: ReplyMode,
) -> Result<(), twilight_http::Error> {
    state
        .http
        .interaction(state.application_id)
        .create_response(
            interaction.id,
            &interaction.token,
            &InteractionResponse {
                kind: if mode == ReplyMode::SilentButton {
                    InteractionResponseType::DeferredUpdateMessage
                } else {
                    InteractionResponseType::DeferredChannelMessageWithSource
                },
                data: matches!(mode, ReplyMode::Private | ReplyMode::SilentCommand).then(|| {
                    InteractionResponseDataBuilder::new()
                        .flags(MessageFlags::EPHEMERAL)
                        .build()
                }),
            },
        )
        .await
        .map(|_| ())
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
        .replace('[', "\\[")
        .replace(']', "\\]")
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

#[cfg(test)]
#[path = "../../tests/discord/commands.rs"]
mod tests;
