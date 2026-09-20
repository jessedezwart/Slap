//! A small, self-contained Spotify Web API client.
//!
//! This module knows nothing about playback, yt-dlp or Discord: it reads
//! playlists and hands back [`SpotifyTrack`]s. Turning those into something
//! playable is the audio layer's job.

pub mod client;
pub mod models;
pub mod service;

use thiserror::Error;
use url::Url;

pub use client::SpotifyClient;
pub use models::{SpotifyPlaylist, SpotifyTrack};
pub use service::SpotifyService;

#[derive(Debug, Error)]
pub enum SpotifyError {
    /// Carries a message written for the person who typed the command.
    #[error("{0}")]
    Api(String),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
}

impl SpotifyError {
    fn api(message: impl Into<String>) -> Self {
        Self::Api(message.into())
    }
}

pub fn is_spotify_url(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("open.spotify.com" | "www.open.spotify.com" | "spotify.link")
    )
}

/// Pull the playlist ID out of a share link.
pub fn playlist_id(url: &Url) -> Result<&str, SpotifyError> {
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    // Localized links carry an `intl-xx` prefix before the resource type.
    let parts = if parts.first().is_some_and(|p| p.starts_with("intl-")) {
        &parts[1..]
    } else {
        &parts[..]
    };
    match parts {
        ["playlist", id]
            if id.len() == 22
                && id.bytes().all(|c| c.is_ascii_alphanumeric())
                && matches!(
                    url.host_str(),
                    Some("open.spotify.com" | "www.open.spotify.com")
                ) =>
        {
            Ok(id)
        }
        _ => Err(SpotifyError::api(
            "use a full https://open.spotify.com/playlist/... link; shortened links, albums and individual tracks are not supported",
        )),
    }
}

#[cfg(test)]
#[path = "../tests/spotify/url.rs"]
mod tests;
