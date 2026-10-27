use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum YtDlpError {
    #[error("could not run `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("yt-dlp did not respond within {0:?}")]
    Timeout(Duration),

    #[error("yt-dlp failed ({code}): {stderr}")]
    Failed { code: String, stderr: String },

    #[error("yt-dlp returned no metadata")]
    NoMetadata,

    #[error("could not parse yt-dlp output: {0}")]
    Json(#[from] serde_json::Error),

    #[error("this video has no Opus audio track")]
    NoOpusFormat,

    #[error("yt-dlp did not return a media URL")]
    NoMediaUrl,
}

impl YtDlpError {
    /// Mark yt-dlp errors as fatal if they indicate that yt-dlp itself is broken/misconfigured.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Spawn { .. } | Self::Timeout(_))
    }
}
