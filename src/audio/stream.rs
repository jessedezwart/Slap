//! The byte stream songbird reads a track from.

use std::{
    io::{self, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
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

use super::{
    AudioError,
    download::{RangedDownload, RemoteFile},
};

const MIN_BUFFER_BYTES: usize = 64 * 1024;
const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// Used when yt-dlp reports no bitrate. YouTube's itag 251 (best Opus audio stream) is ~160 kbps.
const ASSUMED_BITRATE_BPS: u32 = 160_000;

/// Convert `buffer` worth of playback into a byte budget for a track.
pub fn budget_bytes(buffer: Duration, bitrate_bps: Option<u32>) -> usize {
    let bitrate_bps = bitrate_bps.unwrap_or(ASSUMED_BITRATE_BPS);
    let per_second = u64::from(bitrate_bps.max(8_000)) / 8;
    let millis = u64::try_from(buffer.as_millis()).unwrap_or(u64::MAX);
    let bytes = per_second * millis / 1000;

    usize::try_from(bytes)
        .unwrap_or(MAX_BUFFER_BYTES)
        .clamp(MIN_BUFFER_BYTES, MAX_BUFFER_BYTES)
}

pub struct HttpStream {
    file: RemoteFile,
    cancel: CancellationToken,
    /// The stream reader that songbird reads from.
    /// It is a wrapper around a `StreamReader` that reads from a `BoxStream` of `Bytes`.
    reader: SyncWrapper<StreamReader<BoxStream<'static, io::Result<Bytes>>, Bytes>>,
    /// How much songbird's adapter should buffer ahead of the mixer.
    budget_bytes: usize,
    pos: u64,
}

impl HttpStream {
    pub async fn open(
        file: RemoteFile,
        budget_bytes: usize,
        cancel: &CancellationToken,
        offset: u64,
    ) -> Result<Self, AudioError> {
        // Playback starts once half the budget is buffered.
        let prefill_bytes = budget_bytes as u64 / 2;

        let mut download = RangedDownload::open(file, cancel.clone(), offset).await?;
        let file = download.file().clone();

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
            cancel: cancel.clone(),
            reader: SyncWrapper::new(StreamReader::new(bytes)),
            budget_bytes,
            pos: offset,
        })
    }
}

impl AsyncRead for HttpStream {
    /// Called by songbird's `AsyncAdapterStream` to fill its ring buffer.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Self gets passed as a Pin as directed by the trait, it needs to be mutable to update the position after reading.
        let me = self.get_mut();
        // How many bytes were filled before the read
        let before: usize = buf.filled().len();
        // Read from the stream reader into the buffer. To make the sync wrapper work, pass the inner reader as a mutex.
        let result = Pin::new(me.reader.get_mut()).poll_read(cx, buf);

        // Update the position based on how many bytes were filled during the read.
        me.pos += (buf.filled().len() - before) as u64;

        result
    }
}

impl AsyncSeek for HttpStream {
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
impl AsyncMediaSource for HttpStream {
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
        let stream = Self::open(self.file.clone(), self.budget_bytes, &self.cancel, offset)
            .await
            .map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
#[path = "../../tests/audio/stream.rs"]
mod tests;
