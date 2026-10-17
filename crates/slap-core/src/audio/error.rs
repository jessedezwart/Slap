use thiserror::Error;

use crate::media::MediaError;

#[derive(Debug, Error)]
pub enum AudioError {
    /// Looking the stream up again (it expired, or was never listed) failed.
    #[error(transparent)]
    Media(#[from] MediaError),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("audio host returned HTTP {0}")]
    HttpStatus(reqwest::StatusCode),

    #[error("the audio pipeline was cancelled")]
    Cancelled,
}
