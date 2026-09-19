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
mod tests {
    use super::*;

    fn query(secs: Option<u64>) -> SearchQuery {
        SearchQuery {
            artists: vec!["Daft Punk".into()],
            title: "Get Lucky".into(),
            duration: secs.map(Duration::from_secs),
        }
    }

    fn found(secs: Option<u64>) -> TrackMetadata {
        TrackMetadata {
            duration: secs.map(Duration::from_secs),
            ..TrackMetadata::default()
        }
    }

    #[test]
    fn accepts_durations_within_tolerance() {
        // 7% of 369s is about 25.8s.
        for secs in [369, 370, 344, 394] {
            assert!(matches(&query(Some(369)), &found(Some(secs))), "{secs}s");
        }
        // Short tracks get at least 10s.
        assert!(matches(&query(Some(60)), &found(Some(70))));
    }

    #[test]
    fn rejects_other_lengths() {
        // The radio edit when the album version was asked for, and
        // SoundCloud's 30-second preview.
        for secs in [248, 30, 343, 395] {
            assert!(!matches(&query(Some(369)), &found(Some(secs))), "{secs}s");
        }
        assert!(!matches(&query(Some(60)), &found(Some(71))));
    }

    #[test]
    fn unknown_durations_are_accepted() {
        assert!(matches(&query(None), &found(Some(30))));
        assert!(matches(&query(Some(369)), &found(None)));
    }
}
