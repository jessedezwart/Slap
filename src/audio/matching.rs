//! Decides whether a search result is the recording that was asked for.
//! Compares duration of requested track and ytdlp search result of the same track.

use std::time::Duration;

use super::ytdlp::TrackMetadata;
use crate::spotify::SpotifyTrack;

pub fn matches(wanted: &SpotifyTrack, found: &TrackMetadata) -> bool {
    match (wanted.duration, found.duration) {
        // If both durations are known, check if they are within tolerance. When no duration is known, we assume it matches.
        (Some(wanted), Some(found)) => wanted.abs_diff(found) <= tolerance(wanted),
        _ => true,
    }
}

/// Uploads vary a few seconds in padding (music video intros for example)
fn tolerance(wanted: Duration) -> Duration {
    // Multiply by 0.07 to allow for a 7% difference in duration, but always allow at least 10 seconds of tolerance.
    Duration::from_secs(10).max(wanted.mul_f64(0.07))
}

#[cfg(test)]
#[path = "../../tests/audio/matching.rs"]
mod tests;
