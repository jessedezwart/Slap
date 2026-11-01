//! What can go wrong handling an interaction.

use crate::{media::MediaError, session::SessionError};

/// These errors are user facing.
#[derive(Debug, thiserror::Error)]
pub(super) enum CommandError {
    #[error("This command only works inside a server.")]
    NotRanFromGuild,

    #[error("Join a voice channel first.")]
    NotInVoice,

    #[error(transparent)]
    Media(#[from] MediaError),

    #[error(transparent)]
    Session(#[from] SessionError),
}
