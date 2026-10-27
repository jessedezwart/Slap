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
use tracing::debug;

use super::{
    RemoteFile,
    download::RangedDownload,
    request::{request_window, total_file_size, window_bytes},
};
use crate::{audio::AudioError, config::BufferConfig};

pub struct HttpSource {
    file: RemoteFile,
    buffer: BufferConfig,
    cancel: CancellationToken,
    /// The downloaded bytes, readable by songbird:
    /// - `BoxStream` because the stream is named by Rust.
    /// - `StreamReader`, because songbird reads with `AsyncRead`, not `Stream`.
    /// - `SyncWrapper`, because songbird requires `Sync`.
    reader: SyncWrapper<StreamReader<BoxStream<'static, io::Result<Bytes>>, Bytes>>,
    /// How much songbird's adapter should buffer ahead of the mixer.
    budget_bytes: usize,
    pos: u64,
}

impl HttpSource {
    pub async fn open(
        mut file: RemoteFile,
        buffer_config: BufferConfig,
        cancel: &CancellationToken,
        offset: u64,
    ) -> Result<Self, AudioError> {
        let budget_bytes = buffer_config.bytes_for(file.bitrate_bps);
        let prefill_bytes = buffer_config.prefill_bytes(budget_bytes);

        let window = window_bytes(budget_bytes);
        let (response, requested) = request_window(&file, offset, window, cancel).await?;

        // Set the file size to the size reported by the server.
        file.content_length = total_file_size(&response).or(file.content_length);

        let mut download = RangedDownload::new(
            file.clone(),
            window,
            cancel.clone(),
            offset,
            response,
            requested,
        );

        // Prefill the buffer
        let mut prefilled = Vec::new();
        let mut buffered = 0;
        while buffered < prefill_bytes {
            // Only load the next chunk if not cancelled.
            let next = tokio::select! {
                () = cancel.cancelled() => return Err(AudioError::Cancelled),
                next = download.next() => next?,
            };
            // If no more bytes are available, stop buffering.
            let Some(bytes) = next else { break };

            buffered += bytes.len() as u64;
            prefilled.push(Ok(bytes));
        }

        // Turn byte vector into a stream, chain it with the rest of the download.
        let bytes = stream::iter(prefilled)
            .chain(download.into_stream())
            .boxed();

        Ok(Self {
            file,
            buffer: buffer_config,
            cancel: cancel.clone(),
            reader: SyncWrapper::new(StreamReader::new(bytes)),
            budget_bytes,
            pos: offset,
        })
    }

    /// Ring buffer size to give songbird's `AsyncAdapterStream`, which does
    /// the actual read-ahead.
    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }
}

impl AsyncRead for HttpSource {
    /// Called by songbird's `AsyncAdapterStream` to fill its ring buffer.
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

impl AsyncSeek for HttpSource {
    /// Called by songbird when the decoder seeks.
    /// Since seeking is not supported, return an error unless the seek is to the current position.
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

    /// Called by songbird after `start_seek`, to get the resulting position.
    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.pos))
    }
}

#[async_trait::async_trait]
impl AsyncMediaSource for HttpSource {
    /// Called by songbird to check if it can seek.
    fn is_seekable(&self) -> bool {
        false
    }

    /// Called by songbird to learn the file size, if known.
    async fn byte_len(&self) -> Option<u64> {
        self.file.content_length
    }

    /// Called by songbird after a read error, to continue from `offset`.
    async fn try_resume(
        &mut self,
        offset: u64,
    ) -> Result<Box<dyn AsyncMediaSource>, AudioStreamError> {
        debug!(offset, expected_bytes = ?self.file.content_length, "resuming audio stream after error");

        let stream = Self::open(self.file.clone(), self.buffer, &self.cancel, offset)
            .await
            .map_err(|error| {
                debug!(offset, %error, "audio stream resume failed");
                AudioStreamError::Fail(Box::new(error))
            })?;

        debug!(offset, "audio stream resumed");
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
#[path = "../../../tests/audio/stream/http.rs"]
mod tests;
