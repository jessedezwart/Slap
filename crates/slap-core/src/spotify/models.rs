//! The parts of Spotify's JSON we actually use.

use std::time::Duration;

use serde_json::Value;

/// A track as Spotify describes it, which is more reliable than any upload's
/// own metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyTrack {
    /// Spotify's own ID. Stable across title edits, so it is what the
    /// resolved-URL cache is keyed on.
    pub id: String,
    pub artists: Vec<String>,
    pub title: String,
    pub duration: Option<Duration>,
    pub album_art: Option<String>,
    pub url: Option<String>,
}

impl SpotifyTrack {
    /// The credited artists, as one line.
    pub fn artist(&self) -> String {
        self.artists.join(", ")
    }
}

#[derive(Debug, Default)]
pub struct SpotifyPlaylist {
    /// The playlist's own name, when the response carried one.
    pub name: Option<String>,
    pub tracks: Vec<SpotifyTrack>,
    /// Entries that were local files, podcasts, or otherwise unplayable.
    pub skipped: usize,
    /// More tracks exist than were returned.
    pub truncated: bool,
}

/// The playlist's own name, from a playlist object.
pub fn playlist_name(body: &Value) -> Option<String> {
    let name = body.get("name").and_then(Value::as_str)?.trim();
    (!name.is_empty()).then(|| name.chars().take(100).collect())
}

/// Read one playlist entry, or `None` if it is not a track we can play.
pub fn track(entry: &Value) -> Option<SpotifyTrack> {
    // `item` is the current API field; older responses used `track`.
    let track = entry.get("item").or_else(|| entry.get("track"))?;
    if entry.get("is_local").and_then(Value::as_bool) == Some(true)
        || track.get("is_local").and_then(Value::as_bool) == Some(true)
        || track.get("type").and_then(Value::as_str) != Some("track")
    {
        return None;
    }
    let title = track.get("name")?.as_str()?.trim();
    let artists: Vec<_> = track
        .get("artists")?
        .as_array()?
        .iter()
        .filter_map(|a| a.get("name")?.as_str())
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_owned)
        .collect();
    if title.is_empty() || artists.is_empty() {
        return None;
    }
    Some(SpotifyTrack {
        id: track.get("id").and_then(Value::as_str)?.to_owned(),
        artists,
        title: title.to_owned(),
        duration: track
            .get("duration_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis),
        // Album art is listed largest first.
        album_art: track
            .pointer("/album/images/0/url")
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: track
            .pointer("/external_urls/spotify")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

#[cfg(test)]
#[path = "../../tests/spotify/models.rs"]
mod tests;
