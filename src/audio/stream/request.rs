use reqwest::{
    Response, StatusCode,
    header::{CONTENT_RANGE, HeaderMap, RANGE},
};
use tokio_util::sync::CancellationToken;

use super::RemoteFile;
use crate::audio::AudioError;

/// Smallest window one request will ask for.
const MIN_WINDOW_BYTES: u64 = 256 * 1024;

/// Total size of a response. Taken from `Content-Range`, otherwise from `Content-Length`.
/// Content-Length will only be used if the server ignored the range and sent the whole body, in which case the stream is completed instantly
/// or if the server is sending the whole file in one go (seems very rare).
pub(super) fn total_file_size(response: &Response) -> Option<u64> {
    if response.status() == StatusCode::PARTIAL_CONTENT {
        total_from_content_range(response.headers())
    } else {
        response.content_length()
    }
}

/// How much one request asks for.
pub(super) fn window_bytes(budget_bytes: usize) -> u64 {
    (budget_bytes as u64).max(MIN_WINDOW_BYTES)
}

/// Issue one ranged request
pub(super) async fn request_window(
    file: &RemoteFile,
    offset: u64,
    window: u64,
    cancel: &CancellationToken,
) -> Result<(Response, u64), AudioError> {
    // Clamp the window to the end of the file when it's known.
    let (range, requested) = window_range(offset, file.content_length, window);

    let request = file
        .client
        .get(&file.url)
        .headers(file.headers.clone())
        .header(RANGE, range);

    let response = tokio::select! {
        // If the cancellation token is triggered, stop.
        () = cancel.cancelled() => return Err(AudioError::Cancelled),
        // Otherwise, finish the request.
        result = request.send() => result?,
    };

    let status = response.status();
    if !status.is_success() {
        return Err(AudioError::HttpStatus(status));
    }

    Ok((response, requested))
}

/// Determine the range header based on the offset, the known content length, and the window size.
fn window_range(offset: u64, content_length: Option<u64>, window: u64) -> (String, u64) {
    let end = match content_length {
        // If the file is known to be longer than the offset, clamp the end to the file length.
        Some(length) if length > offset => offset.saturating_add(window).min(length),
        // Otherwise, just use the window size.
        _ => offset.saturating_add(window),
    };

    // Build the header
    (format!("bytes={offset}-{}", end - 1), end - offset)
}

/// Total size from `Content-Range: bytes <start>-<end>/<total>`.
fn total_from_content_range(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_RANGE)?
        .to_str()
        // If malformed, return None
        .ok()?
        // Split on the last slash
        .rsplit('/')
        // Take the last part
        .next()?
        .trim()
        // Parse as u64 (inferred by option type)
        .parse()
        .ok()
}

#[cfg(test)]
#[path = "../../../tests/audio/stream/request.rs"]
mod tests;
