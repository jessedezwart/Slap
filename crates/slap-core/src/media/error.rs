use thiserror::Error;

use super::{spotify::SpotifyError, ytdlp::YtDlpError};

/// Anything that can go wrong between a link and a playable stream URL.
#[derive(Debug, Error)]
pub enum MediaError {
    #[error("`{0}` is not a valid HTTP(S) link")]
    UnsupportedUrl(String),

    #[error("Spotify: {0}")]
    Spotify(#[from] SpotifyError),

    #[error(transparent)]
    YtDlp(#[from] YtDlpError),

    #[error("no search result matched `{0}`")]
    NoMatch(String),
}

impl MediaError {
    /// Whether yt-dlp itself is unusable, so retrying with another input would
    /// only fail the same way.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::YtDlp(error) => error.is_fatal(),
            _ => false,
        }
    }
}
