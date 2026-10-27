//! Ranged windows stitched into one continuous byte stream.

use std::io;

use bytes::Bytes;
use futures_util::{
    Stream, StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{
    Client, Response, StatusCode,
    header::{CONTENT_RANGE, HeaderMap, RANGE},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::AudioError;
use crate::media::StreamInfo;

/// Bytes per ranged request. A single request for the whole file is what
/// googlevideo paces down to playback rate.
pub const WINDOW_BYTES: u64 = 256 * 1024;

#[derive(Clone)]
pub struct RemoteFile {
    // The shared HTTP client
    pub http: Client,
    // The URL to request from
    pub url: String,
    // The headers to send with the request
    pub headers: HeaderMap,
    /// Length of complete file. Optional since not always provided.
    pub content_length: Option<u64>,
}

impl RemoteFile {
    pub fn new(http: Client, stream: &StreamInfo) -> Self {
        Self {
            http,
            url: stream.media_url.clone(),
            headers: stream.headers.clone(),
            content_length: stream.content_length,
        }
    }
}

/// Downloads the file one ranged window after another.
pub struct RangedDownload {
    file: RemoteFile,
    cancel: CancellationToken,
    /// Next file byte to download.
    offset: u64,
    /// The window being read; `None` once the download has ended.
    body: Option<Body>,
}

/// One window's response body and how far into it we are.
struct Body {
    stream: BoxStream<'static, reqwest::Result<Bytes>>,
    received: u64,
    /// The server honoured the range (206) instead of sending the whole file.
    partial: bool,
}

impl Body {
    /// Track a freshly opened window.
    fn new(response: Response) -> Self {
        Self {
            partial: response.status() == StatusCode::PARTIAL_CONTENT,
            stream: response.bytes_stream().boxed(),
            received: 0,
        }
    }
}

impl RangedDownload {
    /// Request the first window here, so that a dead URL fails before
    /// songbird is handed a track.
    pub async fn open(
        mut file: RemoteFile,
        cancel: CancellationToken,
        offset: u64,
    ) -> Result<Self, AudioError> {
        let response = request_window(&file, offset, &cancel).await?;
        // Set the file size to the size reported by the server.
        file.content_length = total_file_size(&response).or(file.content_length);

        Ok(Self {
            file,
            cancel,
            offset,
            body: Some(Body::new(response)),
        })
    }

    /// The file being downloaded, with the size the server reported.
    pub fn file(&self) -> &RemoteFile {
        &self.file
    }

    /// The rest of the download as a stream, which ends early when `cancel` fires.
    pub fn into_stream(self) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
        let cancel = self.cancel.clone();

        stream::try_unfold(self, |mut download| async move {
            Ok(download.next().await?.map(|bytes| (bytes, download)))
        })
        .take_until(cancel.cancelled_owned())
    }

    /// The next piece of the file, or `None` once it has all arrived.
    pub async fn next(&mut self) -> io::Result<Option<Bytes>> {
        loop {
            let Some(mut body) = self.body.take() else {
                return Ok(None);
            };

            match body.stream.next().await {
                Some(Ok(bytes)) => {
                    body.received += bytes.len() as u64;
                    self.offset += bytes.len() as u64;
                    self.body = Some(body);

                    return Ok(Some(bytes));
                }
                Some(Err(error)) => {
                    debug!(offset = self.offset, %error, "audio response body failed");

                    return Err(io::Error::other(error));
                }
                // The window is done; whether the file is depends on how much
                // of it arrived.
                None => self.body = self.next_window(body.partial, body.received).await?,
            }
        }
    }

    /// Decide whether the file is done and, if not, request the next window.
    /// `partial` and `received` describe the window that just finished.
    async fn next_window(&mut self, partial: bool, received: u64) -> io::Result<Option<Body>> {
        let complete = !partial
            || self
                .file
                .content_length
                .is_some_and(|length| self.offset >= length);
        if complete {
            debug!(offset = self.offset, "audio download finished");
            return Ok(None);
        }

        if received == 0 {
            warn!(offset = self.offset, expected_bytes = ?self.file.content_length,
                "audio response made no progress");
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "empty partial audio response before end of file",
            ));
        }

        match request_window(&self.file, self.offset, &self.cancel).await {
            Ok(response) => Ok(Some(Body::new(response))),
            Err(AudioError::Cancelled) => Ok(None),
            // For an unknown total, probing past the last byte establishes EOF.
            Err(AudioError::HttpStatus(StatusCode::RANGE_NOT_SATISFIABLE))
                if self.file.content_length.is_none() =>
            {
                debug!(offset = self.offset, "audio download finished");
                Ok(None)
            }
            Err(error) => {
                warn!(offset = self.offset, expected_bytes = ?self.file.content_length, %error,
                    "audio download interrupted");
                Err(io::Error::other(error))
            }
        }
    }
}

/// Issue one ranged request
async fn request_window(
    file: &RemoteFile,
    offset: u64,
    cancel: &CancellationToken,
) -> Result<Response, AudioError> {
    // Clamp the window to the end of the file when it's known.
    let range = window_range(offset, file.content_length);

    let request = file
        .http
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

    Ok(response)
}

/// Determine the range header based on the offset, the known content length, and the window size.
fn window_range(offset: u64, content_length: Option<u64>) -> String {
    let end = match content_length {
        // If the file is known to be longer than the offset, clamp the end to the file length.
        Some(length) if length > offset => offset.saturating_add(WINDOW_BYTES).min(length),
        // Otherwise, just use the window size.
        _ => offset.saturating_add(WINDOW_BYTES),
    };

    // Build the header
    format!("bytes={offset}-{}", end - 1)
}

/// Total size of a response. Taken from `Content-Range`, otherwise from `Content-Length`.
/// Content-Length will only be used if the server ignored the range and sent the whole body, in which case the stream is completed instantly
/// or if the server is sending the whole file in one go (seems very rare).
fn total_file_size(response: &Response) -> Option<u64> {
    if response.status() == StatusCode::PARTIAL_CONTENT {
        total_from_content_range(response.headers())
    } else {
        response.content_length()
    }
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
#[path = "../../tests/audio/download.rs"]
mod tests;
