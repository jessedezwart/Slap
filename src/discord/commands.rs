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
    error::{CommandError, user_message},
    form::{PLAY_MODAL, PlayInput, command_input, open_modal, play_input},
};
use crate::{
    discord::{player, state::State},
    voice::session::Control,
};

/// What a player button asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Open the play form.
    Add,
    Queue,
    Control(Control),
}

/// What an interaction asks for.
enum Request {
    /// `/play` without a link, or the Add button: answered with the form.
    OpenForm,
    Play(PlayInput),
    /// The queue button.
    Queue,
    /// A playback button.
    Control(Control),
}

impl Request {
    fn read(interaction: &Interaction) -> Option<Self> {
        match interaction.data.as_ref()? {
            // Without a link the command only opens the form; the
            // submission does the work.
            InteractionData::ApplicationCommand(data) if data.name == "play" => {
                Some(command_input(data).map_or(Self::OpenForm, Self::Play))
            }
            InteractionData::ModalSubmit(data) if data.custom_id == PLAY_MODAL => {
                Some(Self::Play(play_input(data)))
            }
            InteractionData::MessageComponent(data) => {
                Some(match player::button_action(&data.custom_id)? {
                    // Add opens the same form /play does; its submission does the work.
                    Action::Add => Self::OpenForm,
                    Action::Queue => Self::Queue,
                    Action::Control(control) => Self::Control(control),
                })
            }
            _ => None,
        }
    }

    /// The name logs record for it.
    fn name(&self) -> &'static str {
        match self {
            Self::OpenForm => "open_form",
            Self::Play(_) => "play",
            Self::Queue => "queue",
            Self::Control(Control::Pause) => "pause",
            Self::Control(Control::Resume) => "resume",
            Self::Control(Control::Skip) => "skip",
            Self::Control(Control::Stop) => "stop",
        }
    }
}

/// How an interaction is answered.
#[derive(Clone, Copy, PartialEq)]
enum ReplyMode {
    /// A reply everyone in the channel sees.
    Public,
    /// A reply only the caller sees.
    Private,
    /// No reply on success: the player shows the change. Errors go to the
    /// caller privately.
    Silent,
}

/// Buttons and slash commands share authorization, execution, and logging.
#[tracing::instrument(skip_all, fields(
    guild_id = interaction.guild_id.map(|id| id.get()), user_id = interaction.author_id().map(|id| id.get()),
    interaction_id = %interaction.id, command = tracing::field::Empty,
))]
pub async fn handle_interaction(state: Arc<State>, interaction: Interaction) {
    let Some(request) = Request::read(&interaction) else {
        return;
    };
    Span::current().record("command", request.name());
    let mode = match request {
        // The command itself only opens the form; the submission does the work.
        Request::OpenForm => {
            if let Err(error) = open_modal(&state, &interaction).await {
                warn!(%error, "failed to open the play modal");
            }
            return;
        }
        // The player reflects every panel action, so only the queue replies.
        Request::Play(_) => ReplyMode::Public,
        Request::Queue => ReplyMode::Private,
        Request::Control(_) => ReplyMode::Silent,
    };
    let started = Instant::now();
    debug!("interaction received");
    if let Err(error) = defer(&state, &interaction, mode).await {
        warn!(%error, "failed to acknowledge interaction");
        return;
    }
    let result = execute(&state, &interaction, request).await;
    let elapsed_ms = started.elapsed().as_millis();
    match &result {
        Ok(_) => info!(elapsed_ms, "interaction completed"),
        Err(error) => warn!(%error, elapsed_ms, "interaction rejected or failed"),
    }
    if let Err(error) = respond(&state, &interaction, mode, result).await {
        warn!(%error, "failed to send interaction response");
    }
}

async fn execute(
    state: &Arc<State>,
    interaction: &Interaction,
    request: Request,
) -> Result<Reply, CommandError> {
    match request {
        Request::Play(input) => play::play(state, interaction, &input.url, input.mode).await,
        Request::Queue => controls::queue(state, interaction)
            .await
            .map(Reply::message),
        Request::Control(control) => controls::control_playback(state, interaction, control)
            .await
            .map(Reply::message),
        Request::OpenForm => unreachable!("the form is opened instead of deferring"),
    }
}

async fn respond(
    state: &Arc<State>,
    interaction: &Interaction,
    mode: ReplyMode,
    result: Result<Reply, CommandError>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let http = state.http.interaction(state.application_id);
    let mentions = AllowedMentions::default();
    match (mode, result) {
        (ReplyMode::Silent, Ok(_)) => {}
        // A deferred button update targets the player itself: errors must be
        // separate private follow-ups, never overwrite the live player.
        (ReplyMode::Silent, Err(error)) => {
            http.create_followup(&interaction.token)
                .content(&user_message(&error))
                .flags(MessageFlags::EPHEMERAL)
                .allowed_mentions(Some(&mentions))
                .await?;
        }
        (ReplyMode::Public | ReplyMode::Private, result) => {
            let reply = result.unwrap_or_else(|error| Reply::message(user_message(&error)));
            let response = http
                .update_response(&interaction.token)
                .content(Some(&reply.message))
                .allowed_mentions(Some(&mentions))
                .await?;
            if let Some(player) = reply.player
                && let Some(guild_id) = interaction.guild_id
            {
                player::start(
                    Arc::clone(state),
                    guild_id,
                    &player.cancel,
                    response.model().await?,
                    player.notice,
                )
                .await;
            }
        }
    }
    Ok(())
}

struct Reply {
    message: String,
    /// Set when the reply should become the session's player.
    player: Option<NewPlayer>,
}

/// Everything the player task needs.
struct NewPlayer {
    cancel: CancellationToken,
    notice: String,
}

impl Reply {
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
) -> Result<(Id<GuildMarker>, Id<ChannelMarker>), CommandError> {
    let guild_id = interaction.guild_id.ok_or(CommandError::NotInGuild)?;
    let user_id = interaction.author_id().ok_or(CommandError::NotInGuild)?;
    let channel_id = state
        .discord_cache
        .voice_state(user_id, guild_id)
        .map(|voice| voice.channel_id())
        .ok_or(CommandError::NotInVoice)?;
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
                kind: if mode == ReplyMode::Silent {
                    InteractionResponseType::DeferredUpdateMessage
                } else {
                    InteractionResponseType::DeferredChannelMessageWithSource
                },
                data: (mode == ReplyMode::Private).then(|| {
                    InteractionResponseDataBuilder::new()
                        .flags(MessageFlags::EPHEMERAL)
                        .build()
                }),
            },
        )
        .await
        .map(|_| ())
}
