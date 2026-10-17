//! Interaction handling: what each one asks for, running it, and replying.

mod controls;
mod error;
mod form;
mod play;

use std::{sync::Arc, time::Instant};

use tokio_util::sync::CancellationToken;
use tracing::{Span, debug, info, warn};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{AllowedMentions, MessageFlags},
    http::interaction::{InteractionResponse, InteractionResponseType},
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker},
    },
};
use twilight_util::builder::InteractionResponseDataBuilder;

pub use self::form::play_command;
use self::{
    error::{PlayError, user_message},
    form::{PLAY_MODAL, PlayInput, command_input, open_modal, play_input},
};
use crate::{
    discord::{player, state::State},
    voice::session::Control,
};

/// What an interaction asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Play,
    Queue,
    Control(Control),
}

impl Action {
    /// The name logs record for it.
    fn name(self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Queue => "queue",
            Self::Control(Control::Pause) => "pause",
            Self::Control(Control::Resume) => "resume",
            Self::Control(Control::Skip) => "skip",
            Self::Control(Control::Stop) => "stop",
        }
    }
}

/// Buttons and slash commands share authorization, execution, and logging.
#[tracing::instrument(skip_all, fields(
    guild_id = interaction.guild_id.map(|id| id.get()), user_id = interaction.author_id().map(|id| id.get()),
    interaction_id = %interaction.id, command = tracing::field::Empty,
    source = tracing::field::Empty,
))]
pub async fn handle_interaction(state: Arc<State>, interaction: Interaction) {
    let (action, input, button) = match &interaction.data {
        // The command itself only opens the form; the submission does the work.
        Some(InteractionData::ApplicationCommand(data)) if data.name == "play" => {
            Span::current().record("command", "play");
            Span::current().record("source", "slash");
            match command_input(data) {
                Some(input) => (Action::Play, Some(input), false),
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
            (Action::Play, Some(play_input(data)), false)
        }
        Some(InteractionData::MessageComponent(data)) => {
            let Some(action) = player::button_command(&data.custom_id) else {
                return;
            };
            // Add opens the same form /play does; its submission does the work.
            if action == Action::Play {
                Span::current().record("command", "play");
                Span::current().record("source", "button");
                if let Err(error) = open_modal(&state, &interaction).await {
                    warn!(%error, "failed to open the play modal");
                }
                return;
            }
            (action, None, true)
        }
        _ => return,
    };
    let command = action.name();
    Span::current().record("command", command);
    Span::current().record("source", if button { "button" } else { "slash" });
    let mode = ReplyMode::new(action, button);
    let started = Instant::now();
    debug!("player interaction received");
    if let Err(error) = defer(&state, &interaction, mode).await {
        warn!(%error, "failed to acknowledge interaction");
        return;
    }
    let track = if matches!(action, Action::Control(_))
        && let Some(guild_id) = interaction.guild_id
    {
        state.sessions.current_track_title(guild_id).await
    } else {
        None
    };
    let result = execute(&state, &interaction, action, input).await;
    let elapsed_ms = started.elapsed().as_millis();
    match &result {
        Ok(_) => info!(
            guild = %guild_name(&state, &interaction),
            player = player_name(&interaction),
            track = track.as_deref(),
            command,
            elapsed_ms,
            "player interaction completed"
        ),
        Err(error) => warn!(%error, elapsed_ms, "player interaction rejected or failed"),
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
    action: Action,
    input: Option<PlayInput>,
) -> Result<PlayReply, PlayError> {
    match (action, input) {
        (Action::Play, Some(input)) if !input.url.is_empty() => {
            play::play(state, interaction, &input.url, input.mode).await
        }
        (Action::Play, _) => Ok(PlayReply::message("You need to give me a media link.")),
        (Action::Queue, _) => controls::queue(state, interaction)
            .await
            .map(PlayReply::message),
        (Action::Control(control), _) => controls::control_playback(state, interaction, control)
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
    fn new(action: Action, button: bool) -> Self {
        // The player reflects every panel action, so only the queue replies.
        let silent = if button {
            action != Action::Queue
        } else {
            matches!(action, Action::Control(Control::Skip | Control::Stop))
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

/// Everything the player task needs.
struct Panel {
    cancel: CancellationToken,
    notice: String,
}

impl PlayReply {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            player: None,
        }
    }
}

/// The caller's guild and voice channel.
fn caller_channel(
    state: &State,
    interaction: &Interaction,
) -> Result<(Id<GuildMarker>, Id<ChannelMarker>), PlayError> {
    let guild_id = interaction.guild_id.ok_or(PlayError::NotInGuild)?;
    let user_id = interaction.author_id().ok_or(PlayError::NotInGuild)?;
    let channel_id = state
        .cache
        .voice_state(user_id, guild_id)
        .map(|voice| voice.channel_id())
        .ok_or(PlayError::NotInVoice)?;
    Ok((guild_id, channel_id))
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
