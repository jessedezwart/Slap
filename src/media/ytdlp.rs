mod client;
mod parse;
mod resolver;

use std::time::Duration;

use thiserror::Error;

pub use client::YtDlpClient;
pub use resolver::YtDlpResolver;

/// Messages are written for the person who typed the command.
#[derive(Debug, Error)]
pub enum YtDlpError {
    #[error("I could not run `{program}` ({source}). Is yt-dlp installed?")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("yt-dlp did not answer within {0:?}. Try again.")]
    Timeout(Duration),

    /// The details are logged where it happens; users only need to know it failed.
    #[error("Something went wrong getting the media. Try again later.")]
    Failed,

    #[error("Something went wrong getting the media. Try again later.")]
    NoMetadata,

    #[error("could not parse yt-dlp output: {0}")]
    Json(#[from] serde_json::Error),

    #[error("That video has no Opus audio track, so I cannot stream it.")]
    NoOpusFormat,

    #[error("Something went wrong getting the media. Try again later.")]
    NoMediaUrl,
}

impl YtDlpError {
    /// Mark yt-dlp errors as fatal if they indicate that yt-dlp itself is broken/misconfigured.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Spawn { .. } | Self::Timeout(_))
    }
}
