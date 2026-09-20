//! Decides whether a search result is the recording that was asked for.
//!
//! Only the duration is compared. Spotify durations are exact, so it is enough
//! to reject radio edits, extended mixes and 30-second previews. A result or
//! query without a duration is accepted: unknown, not wrong.

use std::time::Duration;

use super::ytdlp::TrackMetadata;

/// A track to find, from metadata that is more reliable than any upload's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub artists: Vec<String>,
    pub title: String,
    pub duration: Option<Duration>,
}

impl SearchQuery {
    /// Free text for the search engines.
    pub fn text(&self) -> String {
        format!("{} {}", self.artists.join(", "), self.title)
    }
}

/// Whether `found` runs about as long as the requested track.
pub fn matches(query: &SearchQuery, found: &TrackMetadata) -> bool {
    match (query.duration, found.duration) {
        (Some(wanted), Some(found)) => wanted.abs_diff(found) <= tolerance(wanted),
        _ => true,
    }
}

/// Uploads vary a few seconds in padding; music videos add intros.
fn tolerance(wanted: Duration) -> Duration {
    Duration::from_secs(10).max(wanted.mul_f64(0.07))
}

#[cfg(test)]
#[path = "../../tests/audio/matching.rs"]
mod tests;
