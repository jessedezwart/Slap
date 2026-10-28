//! The parts of Spotify's JSON we actually use, read straight into
//! [`crate::media`] types.

use std::time::Duration;

use serde_json::Value;

use crate::media::{ImportedTrack, Metadata, SearchQuery, Target, http_url};

pub fn playlist_name(body: &Value) -> Option<String> {
    body.get("name")
        .and_then(Value::as_str)
        .and_then(crate::media::playlist_name)
}

/// Parse a Spotify playlist entry (a track) into an [`ImportedTrack`]
pub fn entry(entry: &Value) -> Option<ImportedTrack> {
    let item = entry.get("item").or_else(|| entry.get("track"))?;
    if entry.get("is_local").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    track(item)
}

/// Parse a Spotify album track into an [`ImportedTrack`], adding the album's thumbnail.
pub fn album_track(item: &Value, album: &Value) -> Option<ImportedTrack> {
    let mut parsed = track(item)?;
    parsed.metadata.thumbnail = album
        .pointer("/images/0/url")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(parsed)
}

/// Parse a Spotify track into an [`ImportedTrack`].
pub fn track(track: &Value) -> Option<ImportedTrack> {
    if track.get("is_local").and_then(Value::as_bool) == Some(true)
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
        .collect();
    if title.is_empty() || artists.is_empty() {
        return None;
    }
    let artist = artists.join(", ");
    let duration = track
        .get("duration_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis);
    Some(ImportedTrack {
        metadata: Metadata {
            title: Some(format!("{artist} — {title}")),
            duration,
            thumbnail: track
                .pointer("/album/images/0/url")
                .and_then(Value::as_str)
                .map(str::to_owned),
            webpage_url: track
                .pointer("/external_urls/spotify")
                .and_then(Value::as_str)
                .and_then(http_url),
        },
        target: Target::Search(SearchQuery {
            key: track.get("id").and_then(Value::as_str)?.to_owned(),
            artist,
            title: title.to_owned(),
            duration,
        }),
        // Spotify has no audio
        stream: None,
    })
}

#[cfg(test)]
#[path = "../../../tests/media/spotify/parse.rs"]
mod tests;
