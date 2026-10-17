//! What can go wrong handling an interaction, and how it is put to the user.

use crate::{
    media::{MediaError, ytdlp::YtDlpError},
    voice::session::SessionError,
};

/// Everything that can go wrong in a way the user should hear about.
#[derive(Debug, thiserror::Error)]
pub(super) enum PlayError {
    #[error("guild only")]
    NotInGuild,

    #[error("caller is not in a voice channel")]
    NotInVoice,

    #[error(transparent)]
    Media(#[from] MediaError),

    #[error("the voice session ended")]
    Cancelled,

    #[error("button pressed on a player that is no longer current")]
    StalePanel,

    #[error(transparent)]
    Session(#[from] SessionError),
}

pub(super) fn user_message(error: &PlayError) -> String {
    match error {
        PlayError::NotInGuild => "This command only works inside a server.".to_owned(),
        PlayError::NotInVoice => "Join a voice channel first.".to_owned(),
        PlayError::Session(SessionError::Join(error)) => {
            format!("I could not join your voice channel: {error}")
        }
        PlayError::Session(SessionError::NotConnected) => {
            "I am not connected to a voice channel in this server.".to_owned()
        }
        PlayError::Session(SessionError::WrongChannel) => {
            "Join my voice channel to control playback.".to_owned()
        }
        PlayError::Session(SessionError::EmptyQueue) => "Nothing is playing.".to_owned(),
        PlayError::Session(SessionError::TrackFinished) => {
            "The track finished before the command could be applied.".to_owned()
        }
        PlayError::Session(SessionError::Ended) | PlayError::StalePanel => {
            "This player has expired; use the current player or a slash command.".to_owned()
        }
        PlayError::Cancelled => "Playback was stopped or the voice session ended.".into(),
        PlayError::Media(MediaError::UnsupportedUrl(_)) => {
            "That does not look like a valid HTTP(S) link.".to_owned()
        }
        PlayError::Media(MediaError::YtDlp(YtDlpError::NoOpusFormat)) => {
            "That video has no Opus audio track, so I cannot stream it.".to_owned()
        }
        PlayError::Media(MediaError::YtDlp(YtDlpError::Spawn { program, .. })) => {
            format!("I could not run `{program}`. Is yt-dlp installed?")
        }
        PlayError::Media(MediaError::YtDlp(YtDlpError::Timeout(_))) => {
            "yt-dlp took too long to answer. Try again.".to_owned()
        }
        other => format!("Something went wrong: {other}"),
    }
}
