//! yt-dlp: a client that runs the program, a parser for the JSON it prints,
//! and the two roles built on top: importing links, and a resolver
//! that turns tracks into audio. Everything it hands out is a
//! [`crate::media`] type.
//!
//! yt-dlp is only ever used to *resolve* a video into a direct media URL plus
//! the headers needed to fetch it; the bytes themselves are pulled by
//! [`crate::audio`]. The process is spawned as argv — there is no shell
//! anywhere in this path.

pub mod client;
mod parse;
pub mod resolver;

use std::{sync::Arc, time::Duration};

use thiserror::Error;
use url::Url;

pub use client::YtDlpClient;
pub use resolver::YtDlpResolver;

use super::{Import, search_cache::SearchCache};
use crate::config::YtDlpConfig;

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

/// yt-dlp plays two roles: listing links, and resolving tracks into audio.
/// Both run the same program, so they share one client. `cache` lets the
/// resolver skip searches resolved on an earlier run.
pub fn build(config: YtDlpConfig, cache: Option<Arc<SearchCache>>) -> (YtDlpClient, YtDlpResolver) {
    let client = YtDlpClient::new(config);
    (client.clone(), YtDlpResolver::new(client, cache))
}

/// List a link: one video, or the entries of a playlist.
pub async fn import(client: &YtDlpClient, url: &Url) -> Result<Import, YtDlpError> {
    let output = client.list(url).await?;
    // A single video comes back with its stream already attached.
    parse::parse_list(&output, url)
}
