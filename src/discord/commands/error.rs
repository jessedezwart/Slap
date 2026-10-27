//! What can go wrong handling an interaction.

use crate::{media::MediaError, session::SessionError};

/// Everything that can go wrong in a way the user should hear about.
/// Messages are written for the person who typed the command.
#[derive(Debug, thiserror::Error)]
pub(super) enum CommandError {
    #[error("This command only works inside a server.")]
    NotInGuild,

    #[error("Join a voice channel first.")]
    NotInVoice,

    #[error(transparent)]
    Media(#[from] MediaError),

    #[error(transparent)]
    Session(#[from] SessionError),
}
