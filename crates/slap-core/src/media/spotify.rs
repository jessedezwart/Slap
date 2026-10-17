//! Spotify: a small Web API client that reads playlists, and a catalog that
//! turns them into [`crate::media`] tracks.
//!
//! Spotify only describes tracks; it has no audio. The resolver searches for
//! something playable once the queue reaches a track.

pub mod catalog;
pub mod client;
pub mod error;
pub mod models;
pub mod service;

use url::Url;

pub use catalog::SpotifyCatalog;
pub use client::SpotifyClient;
pub use error::SpotifyError;
pub use models::{SpotifyPlaylist, SpotifyTrack};
pub use service::SpotifyService;

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
