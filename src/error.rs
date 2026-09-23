//! Error types for the audio pipeline and for start-up configuration.

use std::{sync::Arc, time::Duration};

use thiserror::Error;

/// Anything that can go wrong between a YouTube link and Opus packets on the wire.
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("`{0}` is not a valid HTTP(S) link")]
    UnsupportedUrl(String),

    #[error("Spotify: {0}")]
    Spotify(#[from] crate::spotify::SpotifyError),

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

    #[error(
        "YouTube requires authentication. The bot admin needs to configure or refresh YouTube cookies."
    )]
    CookiesRequired,

    #[error("yt-dlp returned no metadata")]
    NoMetadata,

    #[error("could not parse yt-dlp output: {0}")]
    Json(#[from] serde_json::Error),

    /// The whole pipeline is passthrough-only, so a video without an Opus
    /// rendition cannot be played at all.
    #[error("this video has no Opus audio track, so it cannot be streamed without transcoding")]
    NoOpusFormat,

    #[error("no search result matched `{0}`")]
    NoMatch(String),

    #[error("yt-dlp did not return a media URL")]
    NoMediaUrl,

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("audio host returned HTTP {0}")]
    HttpStatus(reqwest::StatusCode),

    #[error("the audio pipeline was cancelled")]
    Cancelled,

    /// A resolve that several callers were waiting on failed. The cause is
    /// kept intact so [`AudioError::is_fatal`] still sees through it.
    #[error(transparent)]
    Shared(Arc<AudioError>),
}

impl AudioError {
    /// Wrap the error from a resolve shared with other callers, unwrapping it
    /// when this caller turns out to be the last one holding it.
    pub fn shared(error: Arc<Self>) -> Self {
        Arc::try_unwrap(error).unwrap_or_else(Self::Shared)
    }

    pub fn cookies_required(&self) -> bool {
        match self {
            Self::CookiesRequired => true,
            Self::Shared(inner) => inner.cookies_required(),
            _ => false,
        }
    }

    /// Whether yt-dlp itself is unusable, so retrying with another input would
    /// only fail the same way.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::Spawn { .. } | Self::Timeout(_) | Self::CookiesRequired => true,
            Self::Shared(inner) => inner.is_fatal(),
            _ => false,
        }
    }
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
