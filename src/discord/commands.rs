//! Interaction handling: what each one asks for, running it, and replying.

mod controls;
mod error;
mod play;
mod play_modal;

use std::sync::Arc;

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

pub use self::play_modal::play_command;
use self::{
    error::CommandError,
    play_modal::{PLAY_MODAL, PlayInput, command_input, open_modal, play_input},
};
use crate::{discord::player, session::Control, state::State};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    OpenForm,
    Control(Control),
}

#[derive(Debug)]
enum Request {
    Play(PlayInput),
    Button(Action),
}

impl Request {
    fn read(interaction: &Interaction) -> Option<Self> {
        match interaction.data.as_ref()? {
            // /play command
            InteractionData::ApplicationCommand(data) if data.name == "play" => {
                // If no URL was given, open modal, otherwise handle directly.
                Some(command_input(data).map_or(Self::Button(Action::OpenForm), Self::Play))
            }
            // Play modal submitted
            InteractionData::ModalSubmit(data) if data.custom_id == PLAY_MODAL => {
                Some(Self::Play(play_input(data)))
            }
            // Button clicked
            InteractionData::MessageComponent(data) => {
                player::button_action(&data.custom_id).map(Self::Button)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ReplyMode {
    Public,
    Silent,
}

#[tracing::instrument(skip_all, fields(
    guild_id = interaction.guild_id.map(|id| id.get()), user_id = interaction.author_id().map(|id| id.get()),
    interaction_id = %interaction.id, command = tracing::field::Empty,
))]
pub async fn handle_interaction(state: Arc<State>, interaction: Interaction) {
    let Some(request) = Request::read(&interaction) else {
        warn!(%interaction.id, "unhandled interaction");
        return;
    };
    Span::current().record("command", tracing::field::debug(&request));
    let (state, interaction) = (&state, &interaction);
    match request {
        Request::Play(input) => {
            let work = play::play(state, interaction, &input.url, input.mode);
            run(state, interaction, ReplyMode::Public, work).await;
        }
        Request::Button(Action::Control(control)) => {
            let work = async {
                controls::apply_control(state, interaction, control).await?;
                Ok(Reply::default())
            };
            run(state, interaction, ReplyMode::Silent, work).await;
        }
        Request::Button(Action::OpenForm) => {
            if let Err(error) = open_modal(state, interaction).await {
                warn!(%error, "failed to open the play modal");
            }
        }
    }
}

/// Acknowledge the interaction, do `work`, then answer as `mode` says.
async fn run(
    state: &Arc<State>,
    interaction: &Interaction,
    mode: ReplyMode,
    work: impl Future<Output = Result<Reply, CommandError>>,
) {
    debug!("interaction received");
    if let Err(error) = defer(state, interaction, mode).await {
        warn!(%error, "failed to acknowledge interaction");
        return;
    }
    let result = work.await;
    match &result {
        Ok(_) => info!("interaction completed"),
        Err(error) => warn!(%error, "interaction rejected or failed"),
    }
    if let Err(error) = respond(state, interaction, mode, result).await {
        warn!(%error, "failed to send interaction response");
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
        // Errors in silent mode are sent as ephemeral messages.
        (ReplyMode::Silent, Err(error)) => {
            http.create_followup(&interaction.token)
                .content(&error.to_string())
                .flags(MessageFlags::EPHEMERAL)
                .allowed_mentions(Some(&mentions))
                .await?;
        }
        (ReplyMode::Public, result) => {
            let reply = result.unwrap_or_else(|error| Reply::message(error.to_string()));
            let response = http
                .update_response(&interaction.token)
                .content(Some(&reply.message))
                .allowed_mentions(Some(&mentions))
                .await?;
            if let Some(cancel) = reply.player
                && let Some(guild_id) = interaction.guild_id
            {
                player::start(Arc::clone(state), guild_id, &cancel, response.model().await?).await;
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Reply {
    message: String,
    // Player identified by cancellation token.
    player: Option<CancellationToken>,
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
    let guild_id = interaction.guild_id.ok_or(CommandError::NotRanFromGuild)?;
    let user_id = interaction.author_id().ok_or(CommandError::NotRanFromGuild)?;
    let channel_id = state
        .discord_cache
        .voice_state(user_id, guild_id)
        .map(|voice| voice.channel_id())
        .ok_or(CommandError::NotInVoice)?;
    Ok((guild_id, channel_id))
}

// Acknowledge interaction to Discord
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
                data: None,
            },
        )
        .await
        .map(|_| ())
}
