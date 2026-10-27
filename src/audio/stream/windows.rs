//! Ranged windows stitched into one continuous byte stream.

use std::io;

use bytes::Bytes;
use futures_util::{
    Stream, StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Response, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::{
    SourceSpec,
    range::{request_window, response_total},
};
use crate::audio::AudioError;

/// Downloads the file one ranged window after another.
pub(super) struct Windows {
    spec: SourceSpec,
    /// Bytes per ranged request.
    window: u64,
    cancel: CancellationToken,
    /// Next file byte to download.
    offset: u64,
    /// The window being read; `None` once the download has ended.
    body: Option<Body>,
}

/// One window's response body and how far into it we are.
struct Body {
    stream: BoxStream<'static, reqwest::Result<Bytes>>,
    requested: u64,
    received: u64,
    /// The server honoured the range (206) instead of sending the whole file.
    partial: bool,
    started: Instant,
}

impl Windows {
    /// `response` is the first window, already opened by the caller so that a
    /// dead URL fails before songbird is handed a track; every window after it
    /// is requested here.
    pub fn new(
        spec: SourceSpec,
        window: u64,
        cancel: CancellationToken,
        offset: u64,
        response: Response,
        requested: u64,
    ) -> Self {
        let mut windows = Self {
            spec,
            window,
            cancel,
            offset,
            body: None,
        };
        windows.body = Some(windows.start(response, requested));

        windows
    }

    /// The rest of the download as a stream, which ends early when `cancel`
    /// fires.
    pub fn into_stream(self) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
        let cancel = self.cancel.clone();

        stream::try_unfold(self, |mut windows| async move {
            Ok(windows.next().await?.map(|bytes| (bytes, windows)))
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
                    debug!(offset = self.offset, received = body.received, requested = body.requested,
                        %error, "audio response body failed");

                    return Err(io::Error::other(error));
                }
                // The window is done; whether the file is depends on how much
                // of it arrived.
                None => self.body = self.next_window(body).await?,
            }
        }
    }

    /// Track a freshly opened window, adopting the file length it reports.
    fn start(&mut self, response: Response, requested: u64) -> Body {
        let total = response_total(&response).or(self.spec.content_length);
        if total != self.spec.content_length {
            debug!(offset = self.offset, previous_bytes = ?self.spec.content_length,
                effective_bytes = ?total, "audio file length changed");
        }
        self.spec.content_length = total;

        Body {
            partial: response.status() == StatusCode::PARTIAL_CONTENT,
            stream: response.bytes_stream().boxed(),
            requested,
            received: 0,
            started: Instant::now(),
        }
    }

    /// Decide whether the file is done and, if not, request the next window.
    async fn next_window(&mut self, finished: Body) -> io::Result<Option<Body>> {
        let Body {
            requested,
            received,
            partial,
            started,
            ..
        } = finished;
        debug!(offset = self.offset, received, requested, partial,
            expected_bytes = ?self.spec.content_length, elapsed_ms = started.elapsed().as_millis(),
            "audio window finished");

        let stop_reason = if !partial {
            Some("full_response_consumed")
        } else if self
            .spec
            .content_length
            .is_some_and(|length| self.offset >= length)
        {
            Some("known_length_reached")
        } else {
            None
        };
        if let Some(reason) = stop_reason {
            self.stopped(reason);
            return Ok(None);
        }

        if received == 0 {
            warn!(offset = self.offset, expected_bytes = ?self.spec.content_length,
                "audio response made no progress");
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "empty partial audio response before end of file",
            ));
        }
        if received < requested {
            debug!(offset = self.offset, received, requested, expected_bytes = ?self.spec.content_length,
                "short audio window; continuing download");
        }

        match request_window(&self.spec, self.offset, self.window, &self.cancel).await {
            Ok((response, requested)) => Ok(Some(self.start(response, requested))),
            Err(AudioError::Cancelled) => {
                self.stopped("cancelled");
                Ok(None)
            }
            // For an unknown total, probing past the last byte establishes EOF.
            Err(AudioError::HttpStatus(StatusCode::RANGE_NOT_SATISFIABLE))
                if self.spec.content_length.is_none() =>
            {
                self.stopped("range_exhausted");
                Ok(None)
            }
            Err(error) => {
                warn!(offset = self.offset, expected_bytes = ?self.spec.content_length, %error,
                    "audio download interrupted");
                Err(io::Error::other(error))
            }
        }
    }

    fn stopped(&self, reason: &'static str) {
        debug!(offset = self.offset, expected_bytes = ?self.spec.content_length, reason,
            "audio download stopped");
    }
}
