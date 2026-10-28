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
        "Spotify support needs SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET configured by the bot owner"
    )]
    NotConfigured,

    #[error(
        "use a full https://open.spotify.com/playlist/..., /album/... or /track/... link; shortened links are not supported"
    )]
    UnsupportedLink,

    #[error("authorization failed; check the Spotify client credentials")]
    AuthFailed,

    #[error("authorization expired or was revoked; reauthorize the bot")]
    AuthRevoked,

    /// Private playlists and Spotify's own editorial playlists are reported
    /// as missing too.
    #[error(
        "playlist not found — it may be private or one of Spotify's own playlists, which the bot cannot read"
    )]
    PlaylistNotFound,

    /// Spotify only shows a Development Mode app the tracks of playlists its
    /// own account owns or collaborates on.
    #[error(
        "Spotify only lets the bot read playlists made by the bot owner's account; albums and single tracks work for any link"
    )]
    PlaylistNotOwned,

    #[error("track not found — make sure the link is a full https://open.spotify.com/track/... link")]
    TrackNotFound,

    #[error("album not found — make sure the link is a full https://open.spotify.com/album/... link")]
    AlbumNotFound,

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

#[derive(Debug, PartialEq, Eq)]
pub enum Link<'a> {
    Playlist(&'a str),
    Album(&'a str),
    Track(&'a str),
}

/// Pull the resource type and ID out of a share link.
pub fn link(url: &Url) -> Result<Link<'_>, SpotifyError> {
    if !matches!(
        url.host_str(),
        Some("open.spotify.com" | "www.open.spotify.com")
    ) {
        return Err(SpotifyError::UnsupportedLink);
    }
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    // Localized links carry an `intl-xx` prefix before the resource type.
    let parts = if parts.first().is_some_and(|p| p.starts_with("intl-")) {
        &parts[1..]
    } else {
        &parts[..]
    };
    let valid = |id: &str| id.len() == 22 && id.bytes().all(|c| c.is_ascii_alphanumeric());
    match *parts {
        ["playlist", id] if valid(id) => Ok(Link::Playlist(id)),
        ["album", id] if valid(id) => Ok(Link::Album(id)),
        ["track", id] if valid(id) => Ok(Link::Track(id)),
        _ => Err(SpotifyError::UnsupportedLink),
    }
}

#[cfg(test)]
#[path = "../../tests/media/spotify/url.rs"]
mod tests;
