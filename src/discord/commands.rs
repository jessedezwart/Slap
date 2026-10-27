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

pub use self::form::play_command;
use self::{
    error::CommandError,
    form::{PLAY_MODAL, PlayInput, command_input, open_modal, play_input},
};
use crate::{discord::player, session::Control, state::State};

/// What a player button asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Open the play form.
    OpenForm,
    Control(Control),
}

/// What an interaction asks for.
enum Request {
    /// A `/play` link, or a submitted play form.
    Play(PlayInput),
    /// A player button, or `/play` without a link, which opens the form.
    Button(Action),
}

impl Request {
    fn read(interaction: &Interaction) -> Option<Self> {
        match interaction.data.as_ref()? {
            // Without a link the command only opens the form; the
            // submission does the work.
            InteractionData::ApplicationCommand(data) if data.name == "play" => {
                Some(command_input(data).map_or(Self::Button(Action::OpenForm), Self::Play))
            }
            InteractionData::ModalSubmit(data) if data.custom_id == PLAY_MODAL => {
                Some(Self::Play(play_input(data)))
            }
            InteractionData::MessageComponent(data) => {
                player::button_action(&data.custom_id).map(Self::Button)
            }
            _ => None,
        }
    }

    /// The name logs record for it.
    fn name(&self) -> &'static str {
        match self {
            Self::Play(_) => "play",
            Self::Button(Action::OpenForm) => "open_form",
            Self::Button(Action::Control(Control::Pause)) => "pause",
            Self::Button(Action::Control(Control::Resume)) => "resume",
            Self::Button(Action::Control(Control::Skip)) => "skip",
            Self::Button(Action::Control(Control::Stop)) => "stop",
        }
    }
}

/// How an interaction is answered.
#[derive(Clone, Copy, PartialEq)]
enum ReplyMode {
    /// A reply everyone in the channel sees.
    Public,
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
    let (state, interaction) = (&state, &interaction);
    // Each future is only built here; it starts once `run` has deferred.
    match request {
        Request::Play(input) => {
            let work = play::play(state, interaction, &input.url, input.mode);
            run(state, interaction, ReplyMode::Public, work).await;
        }
        // The player reflects every control, so these do not reply.
        Request::Button(Action::Control(control)) => {
            let work = controls::control_playback(state, interaction, control);
            run(state, interaction, ReplyMode::Silent, work).await;
        }
        // The form is the answer; its submission does the work.
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
    let started = Instant::now();
    debug!("interaction received");
    if let Err(error) = defer(state, interaction, mode).await {
        warn!(%error, "failed to acknowledge interaction");
        return;
    }
    let result = work.await;
    let elapsed_ms = started.elapsed().as_millis();
    match &result {
        Ok(_) => info!(elapsed_ms, "interaction completed"),
        Err(error) => warn!(%error, elapsed_ms, "interaction rejected or failed"),
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
        // A deferred button update targets the player itself: errors must be
        // separate private follow-ups, never overwrite the live player.
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
    /// The session's token, set when the reply should become that session's
    /// player. The bid loses when the session already has one.
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
                data: None,
            },
        )
        .await
        .map(|_| ())
}
