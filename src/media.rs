//! Finding what to play: the importer turns links into [`ImportedTrack`]s, the resolver
//! turns a track into a [`StreamInfo`]. Nothing here touches audio bytes;
//! that is [`crate::audio`]'s job.
//!
//! The types below describe media independent of where it came from. yt-dlp
//! and Spotify both produce them; they know nothing about either source, or
//! about the queue.

pub mod importer;
pub mod search_cache;
pub mod spotify;
pub mod ytdlp;

use std::time::{Duration, SystemTime};

use reqwest::header::HeaderMap;
use thiserror::Error;
use url::Url;

use self::{spotify::SpotifyError, ytdlp::YtDlpError};

/// Limit imports so accidental channel/radio links cannot create an unbounded queue.
pub const PLAYLIST_LIMIT: usize = 1000;

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
    /// Whether `media_url` can still be fetched; always, when the host gave
    /// no expiry.
    pub fn is_fresh(&self) -> bool {
        self.expires_at.is_none_or(|expires_at| {
            expires_at
                .duration_since(SystemTime::now())
                .is_ok_and(|left| left > EXPIRY_MARGIN)
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub title: Option<String>,
    pub duration: Option<Duration>,
    pub thumbnail: Option<String>,
    pub webpage_url: Option<Url>,
}

impl Metadata {
    /// The title, else the link, else a placeholder.
    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .or(self.webpage_url.as_ref().map(Url::as_str))
            .unwrap_or("Unknown track")
    }
}

/// One entry of an import.
#[derive(Debug)]
pub struct ImportedTrack {
    /// How to find the audio, now or again later.
    pub target: Target,
    pub metadata: Metadata,
    /// Ready to play, when the source got it for free while listing. Without
    /// one, or once it expires, the resolver looks the track up from `target`.
    pub stream: Option<StreamInfo>,
}

/// The result of importing a link: one track, or the entries of a playlist.
#[derive(Debug, Default)]
pub struct Import {
    pub tracks: Vec<ImportedTrack>,
    /// Entries that were local files, podcasts, or otherwise unplayable.
    pub skipped: usize,
    /// More tracks exist than were returned.
    pub truncated: bool,
    /// Set only when the link was a playlist or album, and it had a name.
    pub playlist_name: Option<String>,
}

/// Searches are kept lazy, just like direct video URLs.
#[derive(Debug)]
pub enum Target {
    Url(Url),
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
    /// Stable identity of the recording in its catalog (e.g. Spotify track ID)
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

/// Anything that can go wrong between a link and a playable stream URL.
/// Messages are written for the person who typed the command.
#[derive(Debug, Error)]
pub enum MediaError {
    #[error("Give a link, or something to search for.")]
    EmptyInput,

    #[error("Spotify: {0}")]
    Spotify(#[from] SpotifyError),

    #[error(transparent)]
    YtDlp(#[from] YtDlpError),

    #[error("No search result matched `{0}`.")]
    NoMatch(String),
}

impl MediaError {
    /// Whether yt-dlp itself is unusable, so retrying with another input would
    /// only fail the same way.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::YtDlp(error) => error.is_fatal(),
            _ => false,
        }
    }
}

pub fn http_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;

    let scheme_ok = matches!(url.scheme(), "http" | "https");
    (scheme_ok && url.host_str().is_some()).then_some(url)
}

/// What the user asked to play: a link, or free text to search for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Link(Url),
    Search(String),
}

impl Input {
    pub fn parse(raw: &str) -> Result<Self, MediaError> {
        if let Some(url) = http_url(raw) {
            return Ok(Self::Link(url));
        }
        let text = raw.trim();
        if text.is_empty() {
            return Err(MediaError::EmptyInput);
        }
        Ok(Self::Search(text.to_owned()))
    }
}

/// A playlist's name as shown to users: trimmed, bounded, and `None` when blank.
pub fn playlist_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (!name.is_empty()).then(|| name.chars().take(100).collect())
}

#[cfg(test)]
#[path = "../tests/media/url.rs"]
mod tests;
