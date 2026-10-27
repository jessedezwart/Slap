//! Spotify: a small Web API client that reads playlists into
//! [`crate::media`] tracks.
//!
//! Spotify only describes tracks; it has no audio. The resolver searches for
//! something playable once the queue reaches a track.

pub mod client;
mod parse;
pub mod service;

use reqwest::StatusCode;
use thiserror::Error;
use url::Url;

pub use client::SpotifyClient;
pub use service::SpotifyService;

/// Messages are written for the person who typed the command.
#[derive(Debug, Error)]
pub enum SpotifyError {
    #[error(
        "playlist support needs SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET configured by the bot owner"
    )]
    NotConfigured,

    #[error(
        "use a full https://open.spotify.com/playlist/... link; shortened links, albums and individual tracks are not supported"
    )]
    UnsupportedLink,

    #[error("authorization failed; check the Spotify client credentials")]
    AuthFailed,

    #[error("authorization expired or was revoked; reauthorize the bot")]
    AuthRevoked,

    /// Without a user token Spotify reports a private playlist as missing,
    /// so both get the same advice.
    #[error(
        "playlist not found — make sure it is set to public, and that the link is a full https://open.spotify.com/playlist/... link"
    )]
    PlaylistNotFound,

    #[error("rate limit reached; try again {}", retry_hint(*.0))]
    RateLimited(Option<u64>),

    #[error("API returned HTTP {0}; try again later")]
    Status(StatusCode),

    #[error("invalid authorization response")]
    InvalidAuthResponse,

    #[error("invalid playlist response")]
    InvalidPlaylistResponse,

    #[error("playlist pagination returned an empty page; try again")]
    EmptyPage,

    #[error("playlist import timed out; try again")]
    Timeout,

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
}

fn retry_hint(seconds: Option<u64>) -> String {
    match seconds {
        Some(seconds) => format!("in {seconds} seconds"),
        None => "later".into(),
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
        _ => Err(SpotifyError::UnsupportedLink),
    }
}

#[cfg(test)]
#[path = "../../tests/media/spotify/url.rs"]
mod tests;
