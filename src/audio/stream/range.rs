//! Ranged HTTP windows: what to ask for, and how big the file really is.

use reqwest::{
    Response, StatusCode,
    header::{CONTENT_RANGE, HeaderMap, RANGE},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::SourceSpec;
use crate::audio::AudioError;

/// Smallest window one ranged request will ask for.
///
/// Measured against googlevideo, every bounded range — 128 KiB through 2 MiB —
/// came back in the same ~40 ms, so window size buys nothing but fewer round
/// trips. Past this floor a *smaller* window is better: the window is what the
/// server holds open while the buffer drains, and a window far larger than the
/// buffer leaves a connection stalled for minutes waiting on the mixer.
const MIN_WINDOW_BYTES: u64 = 256 * 1024;

/// A partial response's Content-Length is only its window size.
pub(super) fn response_total(response: &Response) -> Option<u64> {
    if response.status() == StatusCode::PARTIAL_CONTENT {
        total_from_content_range(response.headers())
    } else {
        response.content_length()
    }
}

/// How much one request asks for.
///
/// Sized from the buffer so that a window is drained in roughly one buffer's
/// worth of playback. That keeps the request bounded — which is all
/// googlevideo needs to serve it at link speed instead of pacing it — without
/// leaving a connection open and stalled long after the buffer has filled.
pub(super) fn window_bytes(budget_bytes: usize) -> u64 {
    (budget_bytes as u64).max(MIN_WINDOW_BYTES)
}

/// Issue one ranged request, returning the response and how many bytes it
/// should carry.
pub(super) async fn request_window(
    spec: &SourceSpec,
    offset: u64,
    window: u64,
    cancel: &CancellationToken,
) -> Result<(Response, u64), AudioError> {
    let (range, requested) = window_range(offset, spec.content_length, window);

    let started = Instant::now();
    debug!(offset, requested, %range, expected_bytes = ?spec.content_length, "requesting audio window");
    let request = spec
        .client
        .get(&spec.url)
        .headers(spec.headers.clone())
        .header(RANGE, range);

    let response = tokio::select! {
        () = cancel.cancelled() => return Err(AudioError::Cancelled),
        result = request.send() => result?,
    };

    // 206 when the range was served, 200 if the server ignored it and sent the
    // whole body — the download finishes after consuming that response.
    let status = response.status();
    debug!(offset, requested, %status,
        content_range = ?response.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()),
        response_bytes = ?response.content_length(), total_bytes = ?response_total(&response),
        elapsed_ms = started.elapsed().as_millis(), "audio window response");
    if !status.is_success() {
        return Err(AudioError::HttpStatus(status));
    }

    Ok((response, requested))
}

/// `Range` header for the window at `offset`, and the byte count it asks for.
fn window_range(offset: u64, content_length: Option<u64>, window: u64) -> (String, u64) {
    // Exclusive end, clamped to the end of the file when we know where that is.
    let end = match content_length {
        Some(length) if length > offset => offset.saturating_add(window).min(length),
        _ => offset.saturating_add(window),
    };

    (format!("bytes={offset}-{}", end - 1), end - offset)
}

/// Total size from `Content-Range: bytes <start>-<end>/<total>`.
///
/// The total is `*` when the server will not say, which fails to parse and
/// leaves us on yt-dlp's number.
fn total_from_content_range(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_RANGE)?
        .to_str()
        .ok()?
        .rsplit('/')
        .next()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
#[path = "../../../tests/audio/stream/range.rs"]
mod tests;
