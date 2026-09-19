//! The `/play` slash command.

use std::{fmt::Write as _, sync::Arc, time::Duration};

use tracing::{info, warn};
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{Interaction, InteractionData, application_command::CommandOptionValue},
    },
    channel::message::MessageFlags,
    http::interaction::{InteractionResponse, InteractionResponseType},
};
use twilight_util::builder::{
    InteractionResponseDataBuilder,
    command::{CommandBuilder, StringBuilder},
};

use crate::{
    audio::{source::OpusYtSource, ytdlp::YtDlp},
    discord::state::State,
    error::AudioError,
    voice::session::SessionError,
};

/// The command definition registered with Discord at start-up.
pub fn play_command() -> Command {
    CommandBuilder::new(
        "play",
        "Play audio from a link in your voice channel",
        CommandType::ChatInput,
    )
    .option(
        StringBuilder::new("url", "An HTTP(S) media link")
            .required(true)
            .build(),
    )
    .build()
}

/// Handle one `/play` interaction from start to finish.
pub async fn handle_interaction(state: Arc<State>, interaction: Interaction) {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return;
    };

    if data.name != "play" {
        return;
    }

    // Resolving metadata takes a second or two, well past Discord's 3s limit
    // for a first response, so acknowledge before doing any work.
    if let Err(error) = defer(&state, &interaction).await {
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

    let message = match url {
        Some(url) => match play(&state, &interaction, url).await {
            Ok(message) => message,
            Err(error) => user_message(&error),
        },
        None => "You need to give me an HTTP(S) media link.".to_owned(),
    };

    if let Err(error) = follow_up(&state, &interaction, &message).await {
        warn!(%error, "failed to send interaction follow-up");
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
) -> Result<String, PlayError> {
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

    let mut source = OpusYtSource::new(
        url,
        state.client.clone(),
        state.ytdlp.clone(),
        state.config.buffer,
        cancel,
    );

    let metadata = source.metadata().await?;

    let (_handle, position) = state
        .sessions
        .enqueue(guild_id, source.into(), metadata.duration)
        .await?;

    let title = metadata.title.clone().unwrap_or_else(|| raw_url.to_owned());

    info!(%guild_id, %channel_id, position, title, "queued track");

    let mut message = if position <= 1 {
        format!("Now playing **{}**", escape(&title))
    } else {
        format!(
            "Queued **{}** at position {}",
            escape(&title),
            position.saturating_sub(1)
        )
    };

    if let Some(duration) = metadata.duration {
        let _ = write!(message, " ({})", format_duration(duration));
    }

    Ok(message)
}

fn user_message(error: &PlayError) -> String {
    match error {
        PlayError::NotInGuild => "This command only works inside a server.".to_owned(),
        PlayError::NotInVoice => "Join a voice channel first, then run `/play` again.".to_owned(),
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

async fn defer(state: &Arc<State>, interaction: &Interaction) -> Result<(), twilight_http::Error> {
    state
        .http
        .interaction(state.application_id)
        .create_response(
            interaction.id,
            &interaction.token,
            &InteractionResponse {
                kind: InteractionResponseType::DeferredChannelMessageWithSource,
                data: None,
            },
        )
        .await
        .map(|_| ())
}

async fn follow_up(
    state: &Arc<State>,
    interaction: &Interaction,
    message: &str,
) -> Result<(), twilight_http::Error> {
    state
        .http
        .interaction(state.application_id)
        .update_response(&interaction.token)
        .content(Some(message))
        .await
        .map(|_| ())
}

/// Stop a track title from breaking out of its bold markers or pinging anyone.
fn escape(text: &str) -> String {
    let trimmed: String = text.chars().take(200).collect();

    trimmed
        .replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('`', "\\`")
        .replace('~', "\\~")
        .replace('|', "\\|")
        .replace('@', "@\u{200b}")
}

fn format_duration(duration: Duration) -> String {
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
