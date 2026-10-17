//! yt-dlp, split the same way as [`crate::spotify`]: a client that runs the
//! program, models for the JSON it prints, and a service that decides what to
//! run it on.
//!
//! yt-dlp is only ever used to *resolve* a video into a direct media URL plus
//! the headers needed to fetch it; the bytes themselves are pulled by
//! [`crate::audio::stream`]. The process is spawned as argv — there is no shell
//! anywhere in this path.

pub mod client;
pub mod models;
pub mod service;

use url::Url;

pub use client::YtDlpClient;
pub use models::{ListedTrack, MediaList, Playlist, Resolved, TrackMetadata};
pub use service::YtDlpService;

use crate::{error::AudioError, spotify::SpotifyTrack};

/// Limit imports so accidental channel/radio links cannot create an unbounded queue.
pub const PLAYLIST_LIMIT: usize = 1000;

/// Searches are kept lazy, just like direct video URLs.
#[derive(Debug)]
pub enum PlaybackTarget {
    Url(Url),
    /// A Spotify track to go and find an upload of.
    Search(SpotifyTrack),
}

impl From<Url> for PlaybackTarget {
    fn from(url: Url) -> Self {
        Self::Url(url)
    }
}

pub fn http_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;

    let scheme_ok = matches!(url.scheme(), "http" | "https");
    (scheme_ok && url.host_str().is_some()).then_some(url)
}

pub fn validate_url(raw: &str) -> Result<Url, AudioError> {
    http_url(raw).ok_or_else(|| AudioError::UnsupportedUrl(raw.to_owned()))
}

#[cfg(test)]
#[path = "../../tests/audio/ytdlp/url.rs"]
mod tests;
