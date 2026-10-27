//! The songbird side: reads buffered chunks and reopens the stream on resume.

use std::{
    io::{self, SeekFrom},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use songbird::input::{AsyncMediaSource, AudioStreamError};
use tokio::{
    io::{AsyncRead, AsyncSeek, ReadBuf},
    sync::{Notify, Semaphore, mpsc},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, warn};

use super::{
    Chunk, MAX_CHUNK_BYTES, SourceSpec,
    pump::Pump,
    range::{request_window, response_total, window_bytes},
};
use crate::{audio::AudioError, config::BufferConfig};

/// Stalls shorter than this are normal scheduling noise and not worth logging.
const STALL_LOG_THRESHOLD: Duration = Duration::from_millis(50);

pub struct PrefetchStream {
    spec: SourceSpec,
    buffer: BufferConfig,
    /// Cancelled when the guild's pipeline is torn down; our own token is a
    /// child of it, so one cancel upstream kills every stream below.
    parent: CancellationToken,
    cancel: CancellationToken,
    rx: mpsc::Receiver<Result<Chunk, io::Error>>,
    task: Option<JoinHandle<()>>,
    ready: Arc<Notify>,
    current: Option<Chunk>,
    cursor: usize,
    pos: u64,
    len: Option<u64>,
    /// Set when a read finds the queue empty: the mixer is being starved.
    starved_since: Option<Instant>,
    span: tracing::Span,
}

impl PrefetchStream {
    /// Open the stream and start filling the buffer.
    ///
    /// The initial request happens here rather than in the background task so
    /// that a dead URL surfaces as an error before songbird is handed a track.
    pub async fn open(
        mut spec: SourceSpec,
        buffer: BufferConfig,
        parent: &CancellationToken,
        offset: u64,
    ) -> Result<Self, AudioError> {
        let cancel = parent.child_token();

        let budget_bytes = buffer.bytes_for(spec.bitrate_bps);
        let prefill_bytes = buffer.prefill_bytes(budget_bytes);

        let window = window_bytes(budget_bytes);
        let (response, requested) = request_window(&spec, offset, window, &cancel).await?;

        // With a *windowed* request `Content-Length` describes the window, not
        // the file, so the total has to come from `Content-Range` or from
        // yt-dlp. The bare `Content-Length` is only meaningful
        // when the server ignored the range and sent the whole body.
        let len = response_total(&response).or(spec.content_length);
        // The pump and subsequent ranges must use the same authoritative size.
        debug!(metadata_bytes = ?spec.content_length, http_bytes = ?response_total(&response),
            effective_bytes = ?len, "selected audio file length");
        spec.content_length = len;

        let budget = Arc::new(Semaphore::new(budget_bytes));
        let ready = Arc::new(Notify::new());

        // The semaphore is the real bound; the channel just needs enough slots
        // not to become the bottleneck.
        let channel_capacity = (budget_bytes / MAX_CHUNK_BYTES).max(4) + 4;
        let (tx, rx) = mpsc::channel(channel_capacity);

        let pump = Pump {
            spec: spec.clone(),
            tx,
            budget,
            ready: Arc::clone(&ready),
            cancel: cancel.clone(),
            window,
            prefill_bytes,
        };
        let task = tokio::spawn(pump.run(response, requested, offset).in_current_span());

        debug!(
            budget_bytes,
            prefill_bytes,
            offset,
            window,
            content_length = ?len,
            "opened prefetch stream"
        );

        Ok(Self {
            spec,
            buffer,
            parent: parent.clone(),
            cancel,
            rx,
            task: Some(task),
            ready,
            current: None,
            cursor: 0,
            pos: offset,
            len,
            starved_since: None,
            span: tracing::Span::current(),
        })
    }

    /// Wait until enough audio is buffered to start playing.
    pub async fn prefill(&mut self) -> Result<(), AudioError> {
        tokio::select! {
            () = self.ready.notified() => Ok(()),
            () = self.cancel.cancelled() => Err(AudioError::Cancelled),
        }
    }
}

impl Drop for PrefetchStream {
    fn drop(&mut self) {
        // Belt and braces: cancelling stops the task cooperatively, aborting
        // covers the case where it is parked inside the HTTP stack.
        self.cancel.cancel();

        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl AsyncRead for PrefetchStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();

        loop {
            if let Some(chunk) = me.current.as_ref() {
                let remaining = &chunk.bytes[me.cursor..];

                if !remaining.is_empty() {
                    let count = remaining.len().min(buf.remaining());
                    buf.put_slice(&remaining[..count]);
                    me.cursor += count;
                    me.pos += count as u64;

                    if let Some(since) = me.starved_since.take() {
                        let stalled = since.elapsed();

                        if stalled >= STALL_LOG_THRESHOLD {
                            warn!(
                                ?stalled,
                                "audio buffer ran dry: playback may have stuttered"
                            );
                        }
                    }

                    return Poll::Ready(Ok(()));
                }

                // Dropping the chunk releases its permits back to the budget,
                // which lets the fetch task pull more.
                me.current = None;
                me.cursor = 0;
            }

            match me.rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(chunk))) => me.current = Some(chunk),
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                // Sender dropped: end of stream.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => {
                    me.starved_since.get_or_insert_with(Instant::now);

                    return Poll::Pending;
                }
            }
        }
    }
}

/// Seeking is not supported: this is a one-shot forward stream.
///
/// [`AsyncMediaSource::try_resume`] is how songbird recovers instead, and it
/// reopens the stream at a byte offset with a fresh ranged request.
impl AsyncSeek for PrefetchStream {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let at_current = match position {
            SeekFrom::Current(0) => true,
            SeekFrom::Start(target) => target == self.pos,
            SeekFrom::End(_) | SeekFrom::Current(_) => false,
        };

        if at_current {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "audio stream is not seekable",
            ))
        }
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.pos))
    }
}

#[async_trait::async_trait]
impl AsyncMediaSource for PrefetchStream {
    fn is_seekable(&self) -> bool {
        false
    }

    async fn byte_len(&self) -> Option<u64> {
        self.len
    }

    async fn try_resume(
        &mut self,
        offset: u64,
    ) -> Result<Box<dyn AsyncMediaSource>, AudioStreamError> {
        debug!(parent: &self.span, offset, expected_bytes = ?self.len, "resuming audio stream after error");

        let mut stream = Self::open(self.spec.clone(), self.buffer, &self.parent, offset)
            .instrument(self.span.clone())
            .await
            .map_err(|error| {
                debug!(parent: &self.span, offset, %error, "audio stream resume failed");
                AudioStreamError::Fail(Box::new(error))
            })?;

        stream.prefill().await.map_err(|error| {
            debug!(parent: &self.span, offset, %error, "audio stream resume prefill failed");
            AudioStreamError::Fail(Box::new(error))
        })?;

        debug!(parent: &self.span, offset, "audio stream resumed");
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
#[path = "../../../tests/audio/stream/reader.rs"]
mod tests;
