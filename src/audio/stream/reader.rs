//! The songbird side: an `AsyncRead` over the window stream that reopens on resume.

use std::{
    io::{self, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use songbird::input::{AsyncMediaSource, AudioStreamError};
use sync_wrapper::SyncWrapper;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tokio_util::{io::StreamReader, sync::CancellationToken};
use tracing::{Instrument, debug};

use super::{
    SourceSpec,
    range::{request_window, response_total, window_bytes},
    windows::Windows,
};
use crate::{audio::AudioError, config::BufferConfig};

pub struct PrefetchStream {
    spec: SourceSpec,
    buffer: BufferConfig,
    /// Cancelled when the guild's pipeline is torn down, which ends the stream.
    cancel: CancellationToken,
    /// songbird wants a `Sync` source; the stream is only ever touched
    /// through `&mut`, so the wrapper costs nothing.
    reader: SyncWrapper<StreamReader<BoxStream<'static, io::Result<Bytes>>, Bytes>>,
    /// How much songbird's adapter should buffer ahead of the mixer.
    budget_bytes: usize,
    pos: u64,
    len: Option<u64>,
    span: tracing::Span,
}

impl PrefetchStream {
    /// Open the stream and download until the prefill target is buffered.
    ///
    /// The initial request happens here rather than lazily so that a dead URL
    /// surfaces as an error before songbird is handed a track.
    pub async fn open(
        mut spec: SourceSpec,
        buffer: BufferConfig,
        cancel: &CancellationToken,
        offset: u64,
    ) -> Result<Self, AudioError> {
        let budget_bytes = buffer.bytes_for(spec.bitrate_bps);
        let prefill_bytes = buffer.prefill_bytes(budget_bytes);

        let window = window_bytes(budget_bytes);
        let (response, requested) = request_window(&spec, offset, window, cancel).await?;

        // With a *windowed* request `Content-Length` describes the window, not
        // the file, so the total has to come from `Content-Range` or from
        // yt-dlp. The bare `Content-Length` is only meaningful
        // when the server ignored the range and sent the whole body.
        let len = response_total(&response).or(spec.content_length);
        // Later windows must use the same authoritative size.
        debug!(metadata_bytes = ?spec.content_length, http_bytes = ?response_total(&response),
            effective_bytes = ?len, "selected audio file length");
        spec.content_length = len;

        let mut windows = Windows::new(
            spec.clone(),
            window,
            cancel.clone(),
            offset,
            response,
            requested,
        );

        // Never hand songbird a track whose buffer is still empty. A short
        // track can end before the target depth is ever reached.
        let mut prefilled = Vec::new();
        let mut buffered = 0u64;
        while buffered < prefill_bytes {
            let next = tokio::select! {
                () = cancel.cancelled() => return Err(AudioError::Cancelled),
                next = windows.next() => next?,
            };
            let Some(bytes) = next else { break };

            buffered += bytes.len() as u64;
            prefilled.push(bytes);
        }

        debug!(
            budget_bytes,
            prefill_bytes,
            buffered,
            offset,
            window,
            content_length = ?len,
            "opened prefetch stream"
        );

        let bytes = stream::iter(prefilled.into_iter().map(Ok::<_, io::Error>))
            .chain(windows.into_stream())
            .boxed();

        Ok(Self {
            spec,
            buffer,
            cancel: cancel.clone(),
            reader: SyncWrapper::new(StreamReader::new(bytes)),
            budget_bytes,
            pos: offset,
            len,
            span: tracing::Span::current(),
        })
    }

    /// Ring buffer size to give songbird's `AsyncAdapterStream`, which does
    /// the actual read-ahead.
    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }
}

impl AsyncRead for PrefetchStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(me.reader.get_mut()).poll_read(cx, buf);
        me.pos += (buf.filled().len() - before) as u64;

        result
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

        let stream = Self::open(self.spec.clone(), self.buffer, &self.cancel, offset)
            .instrument(self.span.clone())
            .await
            .map_err(|error| {
                debug!(parent: &self.span, offset, %error, "audio stream resume failed");
                AudioStreamError::Fail(Box::new(error))
            })?;

        debug!(parent: &self.span, offset, "audio stream resumed");
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
#[path = "../../../tests/audio/stream/reader.rs"]
mod tests;
