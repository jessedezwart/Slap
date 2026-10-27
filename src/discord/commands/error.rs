//! What can go wrong handling an interaction, and how it is put to the user.

use crate::{
    media::{MediaError, ytdlp::YtDlpError},
    voice::session::SessionError,
};

/// Everything that can go wrong in a way the user should hear about.
#[derive(Debug, thiserror::Error)]
pub(super) enum CommandError {
    #[error("guild only")]
    NotInGuild,

    #[error("caller is not in a voice channel")]
    NotInVoice,

    #[error(transparent)]
    Media(#[from] MediaError),

    #[error(transparent)]
    Session(#[from] SessionError),
}

pub(super) fn user_message(error: &CommandError) -> String {
    match error {
        CommandError::NotInGuild => "This command only works inside a server.".to_owned(),
        CommandError::NotInVoice => "Join a voice channel first.".to_owned(),
        CommandError::Session(SessionError::Join(error)) => {
            format!("I could not join your voice channel: {error}")
        }
        CommandError::Session(SessionError::NotConnected) => {
            "I am not connected to a voice channel in this server.".to_owned()
        }
        CommandError::Session(SessionError::WrongChannel) => {
            "Join my voice channel to control playback.".to_owned()
        }
        CommandError::Session(SessionError::EmptyQueue) => "Nothing is playing.".to_owned(),
        CommandError::Session(SessionError::TrackFinished) => {
            "The track finished before the command could be applied.".to_owned()
        }
        CommandError::Session(SessionError::Ended) => {
            "That voice session has ended; use the current player, or /play to start again."
                .to_owned()
        }
        CommandError::Media(MediaError::UnsupportedUrl(_)) => {
            "That does not look like a valid HTTP(S) link.".to_owned()
        }
        CommandError::Media(MediaError::YtDlp(YtDlpError::NoOpusFormat)) => {
            "That video has no Opus audio track, so I cannot stream it.".to_owned()
        }
        CommandError::Media(MediaError::YtDlp(YtDlpError::Spawn { program, .. })) => {
            format!("I could not run `{program}`. Is yt-dlp installed?")
        }
        CommandError::Media(MediaError::YtDlp(YtDlpError::Timeout(_))) => {
            "yt-dlp took too long to answer. Try again.".to_owned()
        }
        other => format!("Something went wrong: {other}"),
    }
}
