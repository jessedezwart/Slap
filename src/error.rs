//! Error types for the audio pipeline and for start-up configuration.

use std::time::Duration;

use thiserror::Error;

/// Anything that can go wrong between a YouTube link and Opus packets on the wire.
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("`{0}` is not a YouTube link")]
    UnsupportedUrl(String),

    #[error("could not run `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("yt-dlp did not respond within {0:?}")]
    Timeout(Duration),

    #[error("yt-dlp failed ({code}): {stderr}")]
    YtDlpFailed { code: String, stderr: String },

    #[error("yt-dlp returned no metadata")]
    NoMetadata,

    #[error("could not parse yt-dlp output: {0}")]
    Json(#[from] serde_json::Error),

    /// The whole pipeline is passthrough-only, so a video without an Opus
    /// rendition cannot be played at all.
    #[error("this video has no Opus audio track, so it cannot be streamed without transcoding")]
    NoOpusFormat,

    #[error("yt-dlp did not return a media URL")]
    NoMediaUrl,

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("audio host returned HTTP {0}")]
    HttpStatus(reqwest::StatusCode),

    #[error("the audio pipeline was cancelled")]
    Cancelled,
}

/// Failures while reading configuration out of the environment.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("required environment variable `{0}` is not set")]
    Missing(&'static str),

    #[error("environment variable `{name}` has an invalid value `{value}`: {reason}")]
    Invalid {
        name: &'static str,
        value: String,
        reason: String,
    },
}
