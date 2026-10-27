//! Parse json output and map to [`crate::media::Import`] and [`crate::media::StreamInfo`].

use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;
use url::Url;

use super::YtDlpError;
use crate::media::{
    Import, ImportedTrack, Metadata, PLAYLIST_LIMIT, StreamInfo, http_url, playlist_name,
};

pub fn parse_list(stdout: &[u8], original: &Url) -> Result<Import, YtDlpError> {
    let listing: Listing = serde_json::from_slice(stdout)?;
    // A playlist has `entries`; a single video is its own full record, audio
    // URL included, so it comes back ready to play.
    let Some(entries) = listing.entries else {
        let stream = parse_stream(stdout)?;
        return Ok(Import {
            tracks: vec![ImportedTrack {
                target: original.clone().into(),
                metadata: stream.metadata.clone(),
                stream: Some(stream),
            }],
            // A lone track belongs to no playlist.
            ..Import::default()
        });
    };
    let mut import = Import {
        truncated: entries.len() > PLAYLIST_LIMIT,
        playlist_name: listing.title.as_deref().and_then(playlist_name),
        ..Import::default()
    };
    for entry in entries.iter().take(PLAYLIST_LIMIT) {
        match Entry::deserialize(entry).ok().and_then(Entry::into_track) {
            Some(track) => import.tracks.push(track),
            None => import.skipped += 1,
        }
    }
    Ok(import)
}

/// Parse a single `yt-dlp -j` record.
pub fn parse_stream(stdout: &[u8]) -> Result<StreamInfo, YtDlpError> {
    // `-j` emits one object per line, so the first value is the record. This
    // also reads it when the output is pretty-printed.
    let output = serde_json::Deserializer::from_slice(stdout)
        .into_iter::<Output>()
        .next()
        .ok_or(YtDlpError::NoMetadata)??;

    // With `-f` selecting one format, yt-dlp reports it either under
    // `requested_downloads` or flattened onto the top level.
    let format = output
        .requested_downloads
        .and_then(|mut d| (!d.is_empty()).then(|| d.swap_remove(0)))
        .filter(|f| f.url.is_some())
        .unwrap_or(output.format);

    let acodec_is_opus = format
        .acodec
        .as_deref()
        .is_some_and(|c| c.to_ascii_lowercase().starts_with("opus"));

    if !acodec_is_opus {
        return Err(YtDlpError::NoOpusFormat);
    }

    let media_url = format.url.ok_or(YtDlpError::NoMediaUrl)?;

    debug!(
        format_id = format.format_id.as_deref().unwrap_or("?"),
        abr = format.abr,
        "resolved opus format"
    );

    Ok(StreamInfo {
        expires_at: signed_url_expiry(&media_url),
        media_url,
        headers: build_headers(format.http_headers.as_ref()),
        // Estimates must never become a download cutoff.
        content_length: format.filesize,
        bitrate_bps: format
            .abr
            .or(format.tbr)
            .filter(|b| *b > 0.0)
            .map(kbps_to_bps),
        metadata: Metadata {
            title: output.title,
            duration: output
                .duration
                .filter(|d| *d > 0.0)
                .map(Duration::from_secs_f64),
            thumbnail: output.thumbnail,
            webpage_url: output.webpage_url.as_deref().and_then(http_url),
        },
    })
}

/// When a signed googlevideo URL stops working, read from its `expire`
/// parameter: `videoplayback?...&expire=1758...`, or the path-segment form the
/// same URLs sometimes take, `/expire/1758.../`.
pub(super) fn signed_url_expiry(media_url: &str) -> Option<SystemTime> {
    let url = Url::parse(media_url).ok()?;
    let secs: u64 = url
        .query_pairs()
        .find(|(name, _)| name == "expire")
        .and_then(|(_, value)| value.parse().ok())
        .or_else(|| {
            let mut segments = url.path_segments()?;
            segments.find(|s| *s == "expire")?;
            segments.next()?.parse().ok()
        })?;
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn kbps_to_bps(kbps: f64) -> u32 {
    (kbps * 1000.0).clamp(0.0, f64::from(u32::MAX)) as u32
}

/// Carry yt-dlp's `http_headers` over to our own client.
///
/// googlevideo cares about the `User-Agent` matching the one used to sign the
/// URL. `Range` is dropped because the stream layer sets its own.
fn build_headers(raw: Option<&BTreeMap<String, String>>) -> HeaderMap {
    let mut headers = HeaderMap::new();

    let Some(raw) = raw else {
        return headers;
    };

    for (name, value) in raw {
        let Ok(name) = HeaderName::try_from(name.as_str()) else {
            continue;
        };

        if name == RANGE {
            continue;
        }

        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    }

    headers
}

/// What `--dump-single-json --flat-playlist` prints: a playlist, or a lone
/// video's full record.
#[derive(Deserialize)]
struct Listing {
    title: Option<String>,
    /// Kept loose, so one odd entry is skipped rather than failing the list.
    entries: Option<Vec<Value>>,
}

/// One flat playlist entry.
#[derive(Deserialize)]
struct Entry {
    url: Option<String>,
    webpage_url: Option<String>,
    title: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    thumbnails: Option<Vec<Thumbnail>>,
    availability: Option<String>,
}

#[derive(Deserialize)]
struct Thumbnail {
    url: Option<String>,
}

impl Entry {
    /// The entry as a track, or `None` when it cannot be played.
    fn into_track(self) -> Option<ImportedTrack> {
        let unavailable = matches!(
            self.availability.as_deref(),
            Some("private" | "premium_only" | "subscriber_only" | "needs_auth")
        );
        if unavailable {
            return None;
        }
        let url = [self.webpage_url.as_deref(), self.url.as_deref()]
            .into_iter()
            .flatten()
            .find_map(http_url)?;
        Some(ImportedTrack {
            metadata: Metadata {
                title: self.title,
                duration: self
                    .duration
                    .filter(|d| d.is_finite() && *d > 0.0)
                    .and_then(|d| Duration::try_from_secs_f64(d).ok()),
                // Flat entries list thumbnails smallest first.
                thumbnail: self
                    .thumbnails
                    .and_then(|thumbnails| thumbnails.into_iter().last())
                    .and_then(|thumbnail| thumbnail.url)
                    .or(self.thumbnail),
                webpage_url: Some(url.clone()),
            },
            target: url.into(),
            // Flat entries carry no audio URL; the resolver finds it later.
            stream: None,
        })
    }
}

#[derive(Deserialize)]
struct Output {
    title: Option<String>,
    duration: Option<f64>,
    webpage_url: Option<String>,
    thumbnail: Option<String>,
    requested_downloads: Option<Vec<Format>>,
    #[serde(flatten)]
    format: Format,
}

#[derive(Deserialize)]
struct Format {
    url: Option<String>,
    acodec: Option<String>,
    abr: Option<f64>,
    tbr: Option<f64>,
    filesize: Option<u64>,
    format_id: Option<String>,
    http_headers: Option<BTreeMap<String, String>>,
}

#[cfg(test)]
#[path = "../../../tests/media/ytdlp/parse.rs"]
mod tests;
