//! Finding what to play: catalogs turn links into [`Track`]s, the resolver
//! turns a track into a [`StreamInfo`]. Nothing here touches audio bytes;
//! that is [`crate::audio`]'s job.
//!
//! The types below describe media independent of where it came from. yt-dlp
//! and Spotify both produce them; they know nothing about either source, or
//! about the queue.

pub mod catalog;
pub mod error;
pub mod matching;
pub mod search_cache;
pub mod spotify;
pub mod ytdlp;

use std::time::{Duration, SystemTime};

pub use error::MediaError;

use reqwest::header::HeaderMap;
use url::Url;

/// A stream URL this close to expiring is treated as expired already, so it
/// cannot run out while it is being fetched.
const EXPIRY_MARGIN: Duration = Duration::from_secs(120);

/// Everything needed to open the audio stream, plus what we show the user.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub media_url: String,
    pub headers: HeaderMap,
    pub content_length: Option<u64>,
    pub bitrate_bps: Option<u32>,
    /// When `media_url` stops working, if the host says so.
    pub expires_at: Option<SystemTime>,
    pub metadata: Metadata,
}

impl StreamInfo {
    /// How long `media_url` stays usable, less a safety margin; `None` when
    /// the host gave no expiry.
    pub fn time_left(&self) -> Option<Duration> {
        let expires_at = self.expires_at?;
        let left = expires_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO);
        Some(left.saturating_sub(EXPIRY_MARGIN))
    }

    /// Whether `media_url` can still be fetched.
    pub fn is_fresh(&self) -> bool {
        self.time_left().is_none_or(|left| !left.is_zero())
    }
}

/// Describes the media itself: what it is, not who asked for it.
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub title: Option<String>,
    pub duration: Option<Duration>,
    pub uploader: Option<String>,
    pub thumbnail: Option<String>,
    pub webpage_url: Option<Url>,
}

/// One entry of an import.
#[derive(Debug)]
pub struct Track {
    /// How to find the audio, now or again later.
    pub target: Target,
    pub metadata: Metadata,
    /// Ready to play, when the catalog got it for free while listing. Without
    /// one, or once it expires, the resolver looks the track up from `target`.
    pub stream: Option<StreamInfo>,
}

/// The result of importing a link: one track, or the entries of a playlist.
#[derive(Debug)]
pub struct Import {
    pub tracks: Vec<Track>,
    pub skipped: usize,
    pub truncated: bool,
    /// Set only when the link was a playlist, and it had a name.
    pub playlist_name: Option<String>,
}

/// Searches are kept lazy, just like direct video URLs.
#[derive(Debug)]
pub enum Target {
    Url(Url),
    /// A recording to go and find an upload of.
    Search(SearchQuery),
}

impl From<Url> for Target {
    fn from(url: Url) -> Self {
        Self::Url(url)
    }
}

/// A recording described by a catalog, to be found on an audio host.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// Stable identity of the recording in its catalog (a Spotify ID, say),
    /// so the upload it resolved to can be remembered across runs.
    pub key: String,
    /// The credited artists, as one line.
    pub artist: String,
    pub title: String,
    /// Used to tell the right upload from extended mixes and live versions.
    pub duration: Option<Duration>,
}

impl SearchQuery {
    /// What to type into a search box.
    pub fn text(&self) -> String {
        format!("{} {}", self.artist, self.title)
    }
}
