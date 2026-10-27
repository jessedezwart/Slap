//! Parse json output and map to [`crate::media::Import`] and [`crate::media::StreamInfo`].

use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE};
use serde::Deserialize;
use tracing::debug;
use url::Url;

use super::{PLAYLIST_LIMIT, YtDlpError, http_url};
use crate::media::{Import, Metadata, StreamInfo, Track};

pub fn parse_list(stdout: &[u8], original: &Url) -> Result<Import, YtDlpError> {
    let value: serde_json::Value = serde_json::from_slice(stdout)?;
    // A playlist has `entries`; a single video is its own full record, audio
    // URL included, so it comes back ready to play.
    let Some(entries) = value.get("entries").and_then(serde_json::Value::as_array) else {
        // A single video comes back with its stream already attached
        let stream = parse_output(stdout)?;
        return Ok(Import {
            tracks: vec![Track {
                target: original.clone().into(),
                metadata: stream.metadata.clone(),
                stream: Some(stream),
            }],
            skipped: 0,
            truncated: false,
            // A lone track belongs to no playlist.
            playlist_name: None,
        });
    };
    let mut tracks = Vec::new();
    let mut skipped = 0;
    for entry in entries.iter().take(PLAYLIST_LIMIT) {
        let unavailable = matches!(
            entry.get("availability").and_then(|v| v.as_str()),
            Some("private" | "premium_only" | "subscriber_only" | "needs_auth")
        );
        let url = ["webpage_url", "url"]
            .into_iter()
            .filter_map(|key| entry.get(key).and_then(|v| v.as_str()))
            .find_map(http_url);
        let Some(url) = url.filter(|_| !unavailable) else {
            skipped += 1;
            continue;
        };
        tracks.push(Track {
            metadata: Metadata {
                title: entry
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                duration: entry
                    .get("duration")
                    .and_then(|v| v.as_f64())
                    .filter(|d| d.is_finite() && *d > 0.0)
                    .and_then(|d| Duration::try_from_secs_f64(d).ok()),
                // Flat entries list thumbnails smallest first.
                thumbnail: entry
                    .get("thumbnails")
                    .and_then(|v| v.as_array())
                    .and_then(|thumbnails| thumbnails.last())
                    .and_then(|thumbnail| thumbnail.get("url"))
                    .or_else(|| entry.get("thumbnail"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                webpage_url: Some(url.clone()),
                ..Metadata::default()
            },
            target: url.into(),
            // Flat entries carry no audio URL; the resolver finds it later.
            stream: None,
        });
    }
    Ok(Import {
        tracks,
        skipped,
        truncated: entries.len() > PLAYLIST_LIMIT,
        playlist_name: playlist_title(&value),
    })
}

/// Parse a single `yt-dlp -j` record.
pub fn parse_output(stdout: &[u8]) -> Result<StreamInfo, YtDlpError> {
    let line = stdout
        .split(|b| *b == b'\n')
        .find(|l| !l.iter().all(u8::is_ascii_whitespace))
        .ok_or(YtDlpError::NoMetadata)?;

    // `-j` emits one object per line, so the first line is the record. Fall
    // back to the whole buffer in case the output is ever pretty-printed.
    let output: Output = match serde_json::from_slice(line) {
        Ok(output) => output,
        Err(line_error) => serde_json::from_slice(stdout).map_err(|_| line_error)?,
    };

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
            // Credited artists beat a "Name - Topic" channel.
            uploader: output
                .artists
                .filter(|a| !a.is_empty())
                .map(|a| a.join(", "))
                .or(output.uploader),
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

/// The playlist's own name, ignoring the placeholders yt-dlp falls back to.
fn playlist_title(value: &serde_json::Value) -> Option<String> {
    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)?
        .trim();
    (!title.is_empty()).then(|| title.chars().take(100).collect())
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

#[derive(Deserialize)]
struct Output {
    title: Option<String>,
    duration: Option<f64>,
    webpage_url: Option<String>,
    uploader: Option<String>,
    artists: Option<Vec<String>>,
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
#[path = "../../../tests/media/ytdlp/dto.rs"]
mod tests;
