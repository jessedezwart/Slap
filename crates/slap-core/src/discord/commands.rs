//! Playback and queue slash commands.

use std::{
    fmt::Write as _,
    sync::Arc,
    time::{Duration, Instant},
};

use rand::seq::SliceRandom;
use songbird::tracks::TrackHandle;
use tokio_util::sync::CancellationToken;
use tracing::{Span, debug, info, warn};
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{
            Interaction, InteractionData,
            application_command::{CommandData, CommandOptionValue},
            modal::{ModalInteractionComponent, ModalInteractionData},
        },
    },
    channel::message::{
        AllowedMentions, Component, MessageFlags,
        component::{
            Label, SelectMenu, SelectMenuOption, SelectMenuType, TextInput, TextInputStyle,
        },
    },
    http::interaction::{InteractionResponse, InteractionResponseType},
};
use twilight_util::builder::{
    InteractionResponseDataBuilder,
    command::{CommandBuilder, StringBuilder},
};

use crate::{
    audio::{
        source::OpusYtSource,
        ytdlp::{
            ListedTrack, MediaList, PLAYLIST_LIMIT, PlaybackTarget, Playlist, TrackMetadata,
            http_url, validate_url,
        },
    },
    discord::{player, state::State},
    error::AudioError,
    spotify::{SpotifyPlaylist, is_spotify_url},
    voice::session::{ControlResult, QueueMode, SessionError},
};

/// Identifies both the modal `/play` opens and the submission it sends back.
const PLAY_MODAL: &str = "play:modal";

/// The only command registered with Discord; everything else is a button.
///
/// Both options are optional: `/play` on its own opens the same form the
/// player's Add button does, so neither entry point needs the other.
pub fn play_command() -> Command {
    CommandBuilder::new(
        "play",
        "Play a track or add a playlist to your voice channel",
        CommandType::ChatInput,
    )
    .option(
        StringBuilder::new("url", "An HTTP(S) track or playlist link")
            .required(false)
            .max_length(1024),
    )
    .option(
        StringBuilder::new(
            "shuffle",
            "Shuffle a playlist before adding it (default: On)",
        )
        .required(false)
        .choices([("On", "on"), ("Off", "off")]),
    )
    .build()
}

/// What the user passed to `/play` directly, if they passed a link at all.
fn command_input(data: &CommandData) -> Option<PlayInput> {
    let value = |name: &str| {
        data.options
            .iter()
            .find(|option| option.name == name)
            .map(|option| &option.value)
    };
    let url = match value("url") {
        Some(CommandOptionValue::String(url)) => url.trim().to_owned(),
        _ => return None,
    };
    Some(PlayInput {
        url,
        // Matching the form: shuffling is what an untouched option does.
        shuffle: !matches!(value("shuffle"), Some(CommandOptionValue::String(value)) if value == "off"),
        // The placement only matters next to an existing queue, which is what
        // the form and the Add button are for.
        mode: QueueMode::Append,
    })
}

/// The form `/play` opens: the link, plus how to order a playlist.
#[allow(deprecated)] // TextInput::label is superseded by the Label component.
fn play_modal() -> InteractionResponse {
    let url = Component::Label(Label {
        id: None,
        label: "Track or playlist link".into(),
        description: Some("An HTTP(S) track or playlist link".into()),
        component: Box::new(Component::TextInput(TextInput {
            id: None,
            custom_id: "url".into(),
            label: None,
            max_length: Some(1024),
            min_length: Some(1),
            placeholder: Some("https://…".into()),
            required: Some(true),
            style: TextInputStyle::Short,
            value: None,
        })),
    });
    let shuffle = Component::Label(Label {
        id: None,
        label: "Shuffle".into(),
        description: Some("Shuffle the imported playlist before adding it".into()),
        component: Box::new(Component::SelectMenu(SelectMenu {
            id: None,
            channel_types: None,
            custom_id: "shuffle".into(),
            default_values: None,
            disabled: false,
            kind: SelectMenuType::Text,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(vec![
                SelectMenuOption {
                    default: true,
                    description: None,
                    emoji: None,
                    label: "On".into(),
                    value: "on".into(),
                },
                SelectMenuOption {
                    default: false,
                    description: None,
                    emoji: None,
                    label: "Off".into(),
                    value: "off".into(),
                },
            ]),
            placeholder: Some("On".into()),
            required: Some(false),
        })),
    });
    let mode = Component::Label(Label {
        id: None,
        label: "If something is already playing".into(),
        description: Some("Where this import goes relative to the current queue".into()),
        component: Box::new(Component::SelectMenu(SelectMenu {
            id: None,
            channel_types: None,
            custom_id: "mode".into(),
            default_values: None,
            disabled: false,
            kind: SelectMenuType::Text,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(vec![
                SelectMenuOption {
                    default: true,
                    description: Some("Play it after everything already queued".into()),
                    emoji: None,
                    label: "Add to the queue".into(),
                    value: "append".into(),
                },
                SelectMenuOption {
                    default: false,
                    description: Some("Jump the queue, after the current track".into()),
                    emoji: None,
                    label: "Play next".into(),
                    value: "next".into(),
                },
                SelectMenuOption {
                    default: false,
                    description: Some("Clear the queue and start this instead".into()),
                    emoji: None,
                    label: "Replace the queue".into(),
                    value: "replace".into(),
                },
            ]),
            placeholder: Some("Add to the queue".into()),
            required: Some(false),
        })),
    });
    InteractionResponse {
        kind: InteractionResponseType::Modal,
        data: Some(
            InteractionResponseDataBuilder::new()
                .custom_id(PLAY_MODAL)
                .title("Play")
                .components([url, shuffle, mode])
                .build(),
        ),
    }
}

/// What the user typed into the modal. Shuffle defaults to on, and an import
/// defaults to the back of the queue.
struct PlayInput {
    url: String,
    shuffle: bool,
    mode: QueueMode,
}

fn play_input(data: &ModalInteractionData) -> PlayInput {
    let mut input = PlayInput {
        url: String::new(),
        shuffle: true,
        mode: QueueMode::Append,
    };
    // Labels wrap one component each; action rows are accepted for safety.
    fn walk(components: &[ModalInteractionComponent], input: &mut PlayInput) {
        for component in components {
            match component {
                ModalInteractionComponent::Label(label) => {
                    walk(std::slice::from_ref(&label.component), input);
                }
                ModalInteractionComponent::ActionRow(row) => walk(&row.components, input),
                ModalInteractionComponent::TextInput(text) if text.custom_id == "url" => {
                    input.url = text.value.trim().to_owned();
                }
                ModalInteractionComponent::StringSelect(select)
                    if select.custom_id == "shuffle" =>
                {
                    // An untouched select submits nothing, which keeps the default.
                    if let Some(value) = select.values.first() {
                        input.shuffle = value != "off";
                    }
                }
                ModalInteractionComponent::StringSelect(select) if select.custom_id == "mode" => {
                    input.mode = match select.values.first().map(String::as_str) {
                        Some("next") => QueueMode::Next,
                        Some("replace") => QueueMode::Replace,
                        _ => QueueMode::Append,
                    };
                }
                _ => {}
            }
        }
    }
    walk(&data.components, &mut input);
    input
}

/// Buttons and slash commands share authorization, execution, and logging.
#[tracing::instrument(skip_all, fields(
    guild_id = interaction.guild_id.map(|id| id.get()), user_id = interaction.author_id().map(|id| id.get()),
    interaction_id = %interaction.id, command = tracing::field::Empty,
    source = tracing::field::Empty,
))]
pub async fn handle_interaction(state: Arc<State>, interaction: Interaction) {
    let (command, input, button) = match &interaction.data {
        // The command itself only opens the form; the submission does the work.
        Some(InteractionData::ApplicationCommand(data)) if data.name == "play" => {
            Span::current().record("command", "play");
            Span::current().record("source", "slash");
            match command_input(data) {
                Some(input) => ("play", Some(input), false),
                // Without a link the command only opens the form; the
                // submission does the work.
                None => {
                    if let Err(error) = open_modal(&state, &interaction).await {
                        warn!(%error, "failed to open the play modal");
                    }
                    return;
                }
            }
        }
        Some(InteractionData::ModalSubmit(data)) if data.custom_id == PLAY_MODAL => {
            ("play", Some(play_input(data)), false)
        }
        Some(InteractionData::MessageComponent(data)) => {
            let Some(command) = player::button_command(&data.custom_id) else {
                return;
            };
            // Add opens the same form /play does; its submission does the work.
            if command == "play" {
                Span::current().record("command", "play");
                Span::current().record("source", "button");
                if let Err(error) = open_modal(&state, &interaction).await {
                    warn!(%error, "failed to open the play modal");
                }
                return;
            }
            (command, None, true)
        }
        _ => return,
    };
    Span::current().record("command", command);
    Span::current().record("source", if button { "button" } else { "slash" });
    let mode = ReplyMode::new(command, button);
    let started = Instant::now();
    debug!("player interaction received");
    if let Err(error) = defer(&state, &interaction, mode).await {
        warn!(%error, "failed to acknowledge interaction");
        return;
    }
    let track = if matches!(command, "skip" | "pause" | "resume" | "stop")
        && let Some(guild_id) = interaction.guild_id
    {
        state.sessions.current_track_title(guild_id).await
    } else {
        None
    };
    let result = execute(&state, &interaction, command, input).await;
    match &result {
        Ok(_) => info!(
            "{}",
            crate::logging::interaction(
                &guild_name(&state, &interaction),
                player_name(&interaction),
                track.as_deref(),
                command,
            )
        ),
        Err(error) => {
            warn!(%error, elapsed_ms = started.elapsed().as_millis(), "player interaction rejected or failed")
        }
    }
    if result.is_ok() {
        debug!(
            elapsed_ms = started.elapsed().as_millis(),
            "player interaction completed"
        );
    }
    if let Err(error) = respond(&state, &interaction, mode, result).await {
        warn!(%error, "failed to send interaction response");
    }
}

fn guild_name(state: &State, interaction: &Interaction) -> String {
    interaction
        .guild_id
        .and_then(|guild_id| {
            state
                .cache
                .guild(guild_id)
                .map(|guild| guild.name().to_owned())
        })
        .unwrap_or_else(|| "Unknown guild".into())
}

fn player_name(interaction: &Interaction) -> &str {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.nick.as_deref())
        .or_else(|| {
            interaction
                .author()
                .and_then(|user| user.global_name.as_deref())
        })
        .or_else(|| interaction.author().map(|user| user.name.as_str()))
        .unwrap_or("Unknown player")
}

async fn execute(
    state: &Arc<State>,
    interaction: &Interaction,
    command: &str,
    input: Option<PlayInput>,
) -> Result<PlayReply, PlayError> {
    match (command, input) {
        ("play-again", _) => replay(state, interaction).await,
        ("play", Some(input)) if !input.url.is_empty() => {
            play(state, interaction, &input.url, input.shuffle, input.mode).await
        }
        ("play", _) => Ok(PlayReply::message(
            "You need to give me an HTTP(S) media link.",
        )),
        _ => control(state, interaction, command)
            .await
            .map(PlayReply::message),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ReplyMode {
    Public,
    Private,
    SilentButton,
    SilentCommand,
}

impl ReplyMode {
    fn new(command: &str, button: bool) -> Self {
        // A replay posts a new public player, not an edit of the stopped one.
        if command == "play-again" {
            return Self::Public;
        }
        // The player reflects every panel action, so only the queue replies.
        let silent = if button {
            command != "queue"
        } else {
            matches!(command, "skip" | "stop")
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
        if let Some(panel) = reply.player
            && let Some(guild_id) = interaction.guild_id
        {
            player::start(
                Arc::clone(state),
                guild_id,
                &panel.cancel,
                response.model().await?,
                panel.notice,
                panel.replay,
            )
            .await;
        }
    }
    Ok(())
}

struct PlayReply {
    message: String,
    player: Option<Panel>,
}

/// Everything the player task needs, including what a later Play again replays.
struct Panel {
    cancel: CancellationToken,
    notice: String,
    replay: player::Replay,
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

/// Turn Spotify's metadata into queue entries.
///
/// What the player displays comes from Spotify, which describes a track more
/// reliably than any upload does; what actually plays is searched for lazily,
/// once the queue reaches the track.
fn playable(playlist: SpotifyPlaylist) -> MediaList {
    let tracks = playlist
        .tracks
        .into_iter()
        .map(|track| {
            let artist = track.artist();
            let metadata = TrackMetadata {
                title: Some(format!("{artist} — {}", track.title)),
                uploader: Some(artist),
                duration: track.duration,
                thumbnail: track.album_art.clone(),
                webpage_url: track.url.as_deref().and_then(http_url),
                ..TrackMetadata::default()
            };
            ListedTrack {
                target: PlaybackTarget::Search(track),
                metadata,
            }
        })
        .collect();
    MediaList {
        tracks,
        skipped: playlist.skipped,
        truncated: playlist.truncated,
        title: playlist.name,
    }
}

async fn play(
    state: &Arc<State>,
    interaction: &Interaction,
    raw_url: &str,
    shuffle: bool,
    mode: QueueMode,
) -> Result<PlayReply, PlayError> {
    let guild_id = interaction.guild_id.ok_or(PlayError::NotInGuild)?;
    let user_id = interaction.author_id().ok_or(PlayError::NotInGuild)?;

    // Reject the link before spawning anything.
    let url = validate_url(raw_url)?;

    let channel_id = state
        .cache
        .voice_state(user_id, guild_id)
        .map(|voice_state| voice_state.channel_id())
        .ok_or(PlayError::NotInVoice)?;

    let warning_channel_id = interaction
        .channel
        .as_ref()
        .ok_or(PlayError::NotInGuild)?
        .id;
    let cancel = state
        .sessions
        .join(guild_id, channel_id, warning_channel_id)
        .await?;

    // Also covers a caller leaving during the join handshake.
    crate::discord::events::reconcile_voice(Arc::clone(state), guild_id);
    let provider = if is_spotify_url(&url) {
        "spotify"
    } else {
        "yt-dlp"
    };
    debug!(
        provider,
        host = url.host_str(),
        shuffle,
        ?mode,
        "loading track or playlist metadata"
    );
    let started = Instant::now();
    let list = async {
        if is_spotify_url(&url) {
            // Spotify only supplies metadata; each track is searched for when
            // the queue gets to it.
            state
                .spotify
                .playlist(&url)
                .await
                .map(playable)
                .map_err(AudioError::from)
        } else {
            state.ytdlp.list(&url).await
        }
    };
    let mut listing = tokio::select! {
        () = cancel.cancelled() => return Err(AudioError::Cancelled.into()),
        result = list => match result {
            Ok(listing) => listing,
            Err(error) => {
                if error.cookies_required() {
                    state.sessions.teardown_if(guild_id, "YouTube cookies required", || !cancel.is_cancelled()).await;
                }
                return Err(error.into());
            }
        },
    };
    debug!(
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
    // One allocation for the whole import, shared by every track of it.
    let playlist = listing.title.take().filter(|_| count > 1).map(|name| {
        Arc::new(Playlist {
            name,
            url: url.clone(),
        })
    });
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
            entry.metadata.playlist = playlist.clone();
            let source = OpusYtSource::new(
                entry.target,
                state.client.clone(),
                state.ytdlp.clone(),
                state.config.buffer,
                cancel.clone(),
            )
            .in_guild(guild_id);
            (source.into(), entry.metadata)
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
            "Added {count} tracks{}{}{}.{}",
            match playlist.as_deref() {
                Some(playlist) => format!(" from {}", player::playlist_link(playlist)),
                None => String::new(),
            },
            if shuffle { " in shuffled order" } else { "" },
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
        player: Some(Panel {
            cancel,
            notice,
            replay: player::Replay {
                url: raw_url.to_owned(),
                shuffle,
                playlist,
            },
        }),
    })
}

/// Restart the link a stopped player was created from.
async fn replay(state: &Arc<State>, interaction: &Interaction) -> Result<PlayReply, PlayError> {
    let guild_id = interaction.guild_id.ok_or(PlayError::NotInGuild)?;
    let message = interaction
        .message
        .as_ref()
        .ok_or(SessionError::StalePanel)?;
    // Take the offer: only the newest stopped player in this guild replays,
    // and only once, so a double press cannot import the list twice.
    let (_, offer) = state
        .replays
        .remove_if(&guild_id, |_, offer| offer.message_id == message.id)
        .ok_or(SessionError::StalePanel)?;
    // Nothing is queued behind a stopped player, so the placement is moot.
    let result = play(
        state,
        interaction,
        &offer.replay.url,
        offer.replay.shuffle,
        QueueMode::Append,
    )
    .await;
    if result.is_ok() {
        // The new player supersedes it; a stopped player is just clutter.
        player::delete_player(state, message.channel_id, message.id).await;
    } else {
        state.replays.insert(guild_id, offer);
    }
    result
}

async fn control(
    state: &Arc<State>,
    interaction: &Interaction,
    command: &str,
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
        .control(guild_id, channel_id, command, 1, panel_message)
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
        "**Current track:** {}\n**Queue:** {} upcoming • page {page}/{pages}",
        label(current),
        total.saturating_sub(1)
    );
    for (offset, track) in tracks.iter().skip(1).take(5).enumerate() {
        let index = 1 + (page - 1) * 5 + offset;
        let _ = write!(message, "\n{index}. {}", label(track));
    }
    if pages > 1 {
        message.push_str("\nOnly the first five upcoming tracks are shown.");
    }
    message
}

fn user_message(error: &PlayError) -> String {
    match error {
        PlayError::NotInGuild => "This command only works inside a server.".to_owned(),
        PlayError::NotInVoice => "Join a voice channel first.".to_owned(),
        PlayError::Session(error) => error.to_string(),
        PlayError::Audio(error) if error.cookies_required() => {
            AudioError::CookiesRequired.to_string()
        }
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

/// A slash `/play` answers with the form instead of a deferred reply.
async fn open_modal(
    state: &Arc<State>,
    interaction: &Interaction,
) -> Result<(), twilight_http::Error> {
    state
        .http
        .interaction(state.application_id)
        .create_response(interaction.id, &interaction.token, &play_modal())
        .await
        .map(|_| ())
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
