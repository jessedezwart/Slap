//! The prefetch buffer that stands between the network and songbird's mixer.
//!
//! songbird's mixer thread reads a packet every 20 ms, synchronously. If that
//! read ever reaches the socket the thread blocks and *every* guild sharing
//! that mixer thread stutters. So a background task pulls bytes as fast as the
//! network allows into a byte-bounded queue, and the mixer only ever reads from
//! that queue.
//!
//! The bound is a [`Semaphore`] holding one permit per buffered byte, so the
//! configured buffer is honoured in bytes rather than in "however large the
//! chunks happened to be". Each chunk carries its permits and releases them
//! when the reader is done with it, which is also the backpressure that stops a
//! long track from being downloaded into memory all at once.

use std::{
    io::{self, SeekFrom},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::{
    Client, StatusCode,
    header::{HeaderMap, RANGE},
};
use songbird::input::{AsyncMediaSource, AudioStreamError};
use tokio::{
    io::{AsyncRead, AsyncSeek, ReadBuf},
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinHandle,
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{config::BufferConfig, error::AudioError};

/// Largest piece we hand to the queue at once. Also caps how many permits a
/// single acquisition needs, so it can never exceed the whole budget.
const MAX_CHUNK_BYTES: usize = 32 * 1024;

/// Stalls shorter than this are normal scheduling noise and not worth logging.
const STALL_LOG_THRESHOLD: Duration = Duration::from_millis(50);

/// Everything needed to open — or reopen — the byte stream.
#[derive(Debug, Clone)]
pub struct SourceSpec {
    pub client: Client,
    pub url: String,
    pub headers: HeaderMap,
    /// googlevideo wants an explicit upper bound (`bytes=0-1023`) rather than
    /// an open-ended range, so we pass yt-dlp's reported filesize through.
    pub content_length: Option<u64>,
    pub bitrate_bps: Option<u32>,
}

/// A buffered chunk. Dropping it returns its bytes to the budget.
struct Chunk {
    bytes: Bytes,
    _permit: OwnedSemaphorePermit,
}

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
}

impl PrefetchStream {
    /// Open the stream and start filling the buffer.
    ///
    /// The initial request happens here rather than in the background task so
    /// that a dead URL surfaces as an error before songbird is handed a track.
    pub async fn open(
        spec: SourceSpec,
        buffer: BufferConfig,
        parent: &CancellationToken,
        offset: u64,
    ) -> Result<Self, AudioError> {
        let cancel = parent.child_token();

        let budget_bytes = buffer.bytes_for(spec.bitrate_bps);
        let prefill_bytes = buffer.prefill_bytes(budget_bytes);

        let mut request = spec.client.get(&spec.url).headers(spec.headers.clone());
        request = request.header(RANGE, range_header(offset, spec.content_length));

        let response = tokio::select! {
            () = cancel.cancelled() => return Err(AudioError::Cancelled),
            result = request.send() => result?,
        };

        let status = response.status();
        if !status.is_success() && status != StatusCode::PARTIAL_CONTENT {
            return Err(AudioError::HttpStatus(status));
        }

        // Prefer what the server actually reports; fall back to yt-dlp's number.
        let len = response
            .content_length()
            .map(|remaining| offset.saturating_add(remaining))
            .or(spec.content_length);

        let budget = Arc::new(Semaphore::new(budget_bytes));
        let ready = Arc::new(Notify::new());

        // The semaphore is the real bound; the channel just needs enough slots
        // not to become the bottleneck.
        let channel_capacity = (budget_bytes / MAX_CHUNK_BYTES).max(4) + 4;
        let (tx, rx) = mpsc::channel(channel_capacity);

        let task = tokio::spawn(pump(
            response.bytes_stream(),
            tx,
            Arc::clone(&budget),
            prefill_bytes,
            Arc::clone(&ready),
            cancel.clone(),
        ));

        debug!(
            budget_bytes,
            prefill_bytes,
            offset,
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
        })
    }

    /// Wait until enough audio is buffered to start playing.
    ///
    /// A timeout here is not fatal: a slow start is better than a failed track,
    /// and the buffer keeps filling regardless.
    pub async fn prefill(&mut self) -> Result<(), AudioError> {
        tokio::select! {
            () = self.ready.notified() => Ok(()),
            () = self.cancel.cancelled() => Err(AudioError::Cancelled),
            () = time::sleep(self.buffer.prefill_timeout) => {
                warn!(
                    timeout = ?self.buffer.prefill_timeout,
                    "buffer did not prefill in time; starting anyway"
                );

                Ok(())
            }
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
        debug!(offset, "resuming audio stream after error");

        let mut stream = Self::open(self.spec.clone(), self.buffer, &self.parent, offset)
            .await
            .map_err(|error| AudioStreamError::Fail(Box::new(error)))?;

        stream
            .prefill()
            .await
            .map_err(|error| AudioStreamError::Fail(Box::new(error)))?;

        Ok(Box::new(stream))
    }
}

/// Pull from the network into the bounded queue until cancelled or exhausted.
async fn pump<S>(
    body: S,
    tx: mpsc::Sender<Result<Chunk, io::Error>>,
    budget: Arc<Semaphore>,
    prefill_bytes: u64,
    ready: Arc<Notify>,
    cancel: CancellationToken,
) where
    S: Stream<Item = reqwest::Result<Bytes>> + Send,
{
    let mut body = Box::pin(body);
    let mut buffered = 0u64;
    let mut announced = false;

    loop {
        let next = tokio::select! {
            () = cancel.cancelled() => break,
            next = body.next() => next,
        };

        let mut bytes = match next {
            Some(Ok(bytes)) => bytes,
            Some(Err(error)) => {
                let _ = tx.send(Err(io::Error::other(error))).await;

                break;
            }
            None => break,
        };

        while !bytes.is_empty() {
            let take = bytes.len().min(MAX_CHUNK_BYTES);
            let piece = bytes.split_to(take);

            // Acquiring permits for the whole piece is the backpressure: once
            // the configured buffer is full this parks until the mixer catches
            // up.
            let permit = tokio::select! {
                () = cancel.cancelled() => return,
                permit = Arc::clone(&budget).acquire_many_owned(take as u32) => match permit {
                    Ok(permit) => permit,
                    Err(_) => return,
                },
            };

            let chunk = Chunk {
                bytes: piece,
                _permit: permit,
            };

            if tx.send(Ok(chunk)).await.is_err() {
                // Reader went away.
                return;
            }

            buffered += take as u64;

            if !announced && buffered >= prefill_bytes {
                announced = true;
                ready.notify_one();
            }
        }
    }

    // Wake anyone still waiting to prefill — a short track can finish before
    // the target depth is ever reached.
    ready.notify_one();
}

fn range_header(offset: u64, content_length: Option<u64>) -> String {
    match content_length {
        Some(length) if length > 0 => format!("bytes={offset}-{}", length - 1),
        _ => format!("bytes={offset}-"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
    };

    use super::*;

    /// A body comfortably larger than the smallest buffer we will allocate, so
    /// the reader has to cycle permits to get through it.
    const BODY_LEN: usize = 512 * 1024;

    fn test_buffer_config() -> BufferConfig {
        BufferConfig {
            // Clamped up to the 64 KiB floor, which is well under BODY_LEN.
            target: Duration::from_millis(1),
            prefill_ratio: 0.5,
            // Long enough that a prefill returning early proves it was the
            // buffer filling, not the timeout firing.
            prefill_timeout: Duration::from_secs(30),
            assumed_bitrate_bps: 160_000,
        }
    }

    fn body() -> Vec<u8> {
        (0..BODY_LEN).map(|i| (i % 251) as u8).collect()
    }

    /// Serve one request: headers, then `first` bytes, then the rest once
    /// `release` fires. Returns the address to point the stream at.
    async fn serve(body: Vec<u8>, first: usize, release: oneshot::Receiver<()>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };

            // Read the request head; we do not care what it says.
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while let Ok(1) = socket.read(&mut byte).await {
                request.push(byte[0]);

                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }

            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n",
                body.len(),
                body.len() - 1,
                body.len()
            );

            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }

            let split = first.min(body.len());

            if socket.write_all(&body[..split]).await.is_err() {
                return;
            }
            let _ = socket.flush().await;

            let _ = release.await;

            let _ = socket.write_all(&body[split..]).await;
            let _ = socket.shutdown().await;
        });

        format!("http://{addr}/audio")
    }

    async fn open_stream(url: String, cancel: &CancellationToken) -> PrefetchStream {
        let spec = SourceSpec {
            client: Client::new(),
            url,
            headers: HeaderMap::new(),
            content_length: Some(BODY_LEN as u64),
            bitrate_bps: Some(160_000),
        };

        PrefetchStream::open(spec, test_buffer_config(), cancel, 0)
            .await
            .expect("stream should open")
    }

    /// Every byte arrives, in order, exactly once.
    ///
    /// The body is many times the buffer budget, so this only completes if
    /// permits are released as chunks are consumed — a stuck budget would hang
    /// here rather than return short.
    #[tokio::test]
    async fn streams_a_body_larger_than_the_buffer() {
        let expected = body();
        let (release, released) = oneshot::channel();
        let url = serve(expected.clone(), 40 * 1024, released).await;

        let cancel = CancellationToken::new();
        let mut stream = open_stream(url, &cancel).await;

        stream.prefill().await.expect("prefill");
        let _ = release.send(());

        let mut actual = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut actual))
            .await
            .expect("read should not stall")
            .expect("read should succeed");

        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual, expected);
    }

    /// Prefill returns once the target depth is buffered, without waiting for
    /// the rest of the body.
    #[tokio::test]
    async fn prefill_returns_before_the_body_is_complete() {
        let (release, released) = oneshot::channel();
        // 40 KiB covers the 32 KiB prefill target for a 64 KiB budget.
        let url = serve(body(), 40 * 1024, released).await;

        let cancel = CancellationToken::new();
        let mut stream = open_stream(url, &cancel).await;

        tokio::time::timeout(Duration::from_secs(5), stream.prefill())
            .await
            .expect("prefill should complete without the rest of the body")
            .expect("prefill should succeed");

        let _ = release.send(());
    }

    /// Cancelling the guild token stops the fetch task.
    #[tokio::test]
    async fn cancelling_the_parent_token_stops_the_pump() {
        let (release, released) = oneshot::channel();
        let url = serve(body(), 40 * 1024, released).await;

        let cancel = CancellationToken::new();
        let mut stream = open_stream(url, &cancel).await;

        stream.prefill().await.expect("prefill");

        // The guild is torn down while the body is still arriving.
        cancel.cancel();

        let task = stream.task.take().expect("task handle");
        let _ = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("pump task should finish promptly after cancellation");

        assert!(stream.cancel.is_cancelled());

        let _ = release.send(());
    }

    /// Dropping the stream cancels its token, so nothing outlives it.
    #[tokio::test]
    async fn dropping_the_stream_cancels_its_token() {
        let (release, released) = oneshot::channel();
        let url = serve(body(), 40 * 1024, released).await;

        let cancel = CancellationToken::new();
        let stream = open_stream(url, &cancel).await;
        let child = stream.cancel.clone();

        drop(stream);

        assert!(child.is_cancelled());
        assert!(!cancel.is_cancelled(), "parent must be left alone");

        let _ = release.send(());
    }

    #[test]
    fn range_uses_explicit_end_when_length_is_known() {
        assert_eq!(range_header(0, Some(1024)), "bytes=0-1023");
        assert_eq!(range_header(512, Some(1024)), "bytes=512-1023");
    }

    #[test]
    fn range_is_open_ended_without_a_length() {
        assert_eq!(range_header(0, None), "bytes=0-");
        assert_eq!(range_header(64, Some(0)), "bytes=64-");
    }
}
