//! Reads from queue and writes to the mixer.

use std::{io, sync::Arc};

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{Response, StatusCode};
use tokio::{
    sync::{Notify, Semaphore, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::{
    Chunk, MAX_CHUNK_BYTES, SourceSpec,
    range::{request_window, response_total},
};
use crate::audio::AudioError;

/// What the download needs; built by [`super::PrefetchStream::open`].
pub(super) struct Pump {
    pub spec: SourceSpec,
    pub tx: mpsc::Sender<Result<Chunk, io::Error>>,
    /// One permit per byte the buffer may hold.
    pub budget: Arc<Semaphore>,
    /// Fired once `prefill_bytes` are buffered, or the download ends.
    pub ready: Arc<Notify>,
    pub cancel: CancellationToken,
    /// Bytes per ranged request.
    pub window: u64,
    pub prefill_bytes: u64,
}

impl Pump {
    /// Pull from the network into the bounded queue until cancelled or exhausted.
    ///
    /// `response` is the first window, already opened by `open` so that a dead
    /// URL fails before songbird is handed a track; every window after it is
    /// requested here.
    pub async fn run(self, response: Response, requested: u64, offset: u64) {
        let Self {
            spec,
            tx,
            budget,
            ready,
            cancel,
            window,
            prefill_bytes,
        } = self;
        let mut download = Download {
            spec,
            tx,
            budget,
            ready,
            cancel,
            window,
            prefill_bytes,
            offset,
            buffered: 0,
            announced: false,
        };

        // Cancelled, or the reader is gone: nobody is left to wake.
        if download.all_windows(response, requested).await.is_err() {
            return;
        }
        // Wake anyone still waiting to prefill — a short track can finish
        // before the target depth is ever reached.
        download.ready.notify_one();
    }
}

/// A running download: the settings plus how far it has got.
struct Download {
    spec: SourceSpec,
    tx: mpsc::Sender<Result<Chunk, io::Error>>,
    budget: Arc<Semaphore>,
    ready: Arc<Notify>,
    cancel: CancellationToken,
    window: u64,
    prefill_bytes: u64,
    /// Next file byte to download.
    offset: u64,
    /// Bytes queued so far.
    buffered: u64,
    /// Whether `ready` has fired for the prefill.
    announced: bool,
}

/// The download was cancelled or its reader dropped; stop without a word.
struct Abandoned;

/// How one window went.
struct Window {
    requested: u64,
    received: u64,
    /// The body ended cleanly rather than with an error.
    completed: bool,
    /// The server honoured the range (206) instead of sending the whole file.
    partial: bool,
}

impl Download {
    /// Drain window after window until the file ends or fails.
    async fn all_windows(
        &mut self,
        mut response: Response,
        mut requested: u64,
    ) -> Result<(), Abandoned> {
        loop {
            let window = self.drain(response, requested).await?;
            match self.next_window(window).await? {
                Some(next) => (response, requested) = next,
                None => return Ok(()),
            }
        }
    }

    /// Read one window's body into the queue.
    async fn drain(&mut self, response: Response, requested: u64) -> Result<Window, Abandoned> {
        let started = Instant::now();
        let partial = response.status() == StatusCode::PARTIAL_CONTENT;
        let total = response_total(&response).or(self.spec.content_length);
        if total != self.spec.content_length {
            debug!(offset = self.offset, previous_bytes = ?self.spec.content_length,
                effective_bytes = ?total, "audio file length changed");
        }
        self.spec.content_length = total;
        let mut body = Box::pin(response.bytes_stream());
        let mut received = 0u64;

        let completed = loop {
            let next = tokio::select! {
                () = self.cancel.cancelled() => return Err(self.stopped(received, "cancelled")),
                next = body.next() => next,
            };

            let bytes = match next {
                Some(Ok(bytes)) => bytes,
                Some(Err(error)) => {
                    debug!(offset = self.offset, received, requested, %error, "audio response body failed");
                    let _ = self.tx.send(Err(io::Error::other(error))).await;

                    break false;
                }
                // The window is done; whether the file is depends on how much
                // of it arrived.
                None => break true,
            };

            received += bytes.len() as u64;
            self.enqueue(bytes, received).await?;
        };

        debug!(offset = self.offset, received, requested, completed, partial,
            expected_bytes = ?self.spec.content_length, elapsed_ms = started.elapsed().as_millis(),
            "audio window finished");
        self.offset = self.offset.saturating_add(received);

        Ok(Window {
            requested,
            received,
            completed,
            partial,
        })
    }

    /// Split `bytes` into chunks and queue them, waiting for room in the budget.
    async fn enqueue(&mut self, mut bytes: Bytes, received: u64) -> Result<(), Abandoned> {
        while !bytes.is_empty() {
            let take = bytes.len().min(MAX_CHUNK_BYTES);
            let piece = bytes.split_to(take);

            // Acquiring permits for the whole piece is the backpressure: once
            // the configured buffer is full this parks until the mixer catches
            // up.
            let permit = tokio::select! {
                () = self.cancel.cancelled() => return Err(self.stopped(received, "cancelled")),
                permit = Arc::clone(&self.budget).acquire_many_owned(take as u32) => {
                    permit.map_err(|_| Abandoned)?
                }
            };

            let chunk = Chunk {
                bytes: piece,
                _permit: permit,
            };

            if self.tx.send(Ok(chunk)).await.is_err() {
                return Err(self.stopped(received, "reader_dropped"));
            }

            self.buffered += take as u64;

            if !self.announced && self.buffered >= self.prefill_bytes {
                self.announced = true;
                self.ready.notify_one();
            }
        }

        Ok(())
    }

    /// Decide whether the file is done and, if not, request the next window.
    async fn next_window(&mut self, window: Window) -> Result<Option<(Response, u64)>, Abandoned> {
        let Window {
            requested,
            received,
            completed,
            partial,
        } = window;

        let stop_reason = if !completed {
            Some("body_error")
        } else if !partial {
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
            debug!(offset = self.offset, buffered = self.buffered,
                expected_bytes = ?self.spec.content_length, reason, "audio download stopped");
            return Ok(None);
        }
        if received > 0 && received < requested {
            debug!(offset = self.offset, received, requested, expected_bytes = ?self.spec.content_length,
                "short audio window; continuing download");
        }

        if received == 0 {
            warn!(offset = self.offset, expected_bytes = ?self.spec.content_length,
                "audio response made no progress");
            let _ = self
                .tx
                .send(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "empty partial audio response before end of file",
                )))
                .await;
            return Ok(None);
        }

        match request_window(&self.spec, self.offset, self.window, &self.cancel).await {
            Ok(next) => Ok(Some(next)),
            Err(AudioError::Cancelled) => {
                debug!(
                    offset = self.offset,
                    buffered = self.buffered,
                    reason = "cancelled",
                    "audio download stopped"
                );
                Err(Abandoned)
            }
            // For an unknown total, probing past the last byte establishes EOF.
            Err(AudioError::HttpStatus(StatusCode::RANGE_NOT_SATISFIABLE))
                if self.spec.content_length.is_none() =>
            {
                debug!(
                    offset = self.offset,
                    buffered = self.buffered,
                    reason = "range_exhausted",
                    "audio download stopped"
                );
                Ok(None)
            }
            Err(error) => {
                warn!(offset = self.offset, expected_bytes = ?self.spec.content_length, %error,
                    "audio download interrupted");
                let _ = self.tx.send(Err(io::Error::other(error))).await;

                Ok(None)
            }
        }
    }

    /// Log why the download stopped mid-window.
    fn stopped(&self, received: u64, reason: &'static str) -> Abandoned {
        debug!(
            offset = self.offset,
            received,
            buffered = self.buffered,
            reason,
            "audio download stopped"
        );
        Abandoned
    }
}
