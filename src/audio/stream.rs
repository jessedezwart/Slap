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
//!
//! "As fast as the network allows" needs one thing from the request shape:
//! googlevideo paces a request for a whole file down to roughly playback rate,
//! so a long track arrives in real time and the buffer can never rebuild after
//! a hiccup. A *bounded* range gets the fast path instead — measured on a 229
//! MiB track, one request for the lot ran at 36 KB/s while a 2 MiB window ran
//! at 45 MB/s — so the body is fetched as a series of ranged windows rather
//! than one open request. This is the same trick as yt-dlp's
//! `--http-chunk-size`.

use std::{
    io::{self, SeekFrom},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{
    Client, Response, StatusCode,
    header::{CONTENT_RANGE, HeaderMap, RANGE},
};
use songbird::input::{AsyncMediaSource, AudioStreamError};
use tokio::{
    io::{AsyncRead, AsyncSeek, ReadBuf},
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinHandle,
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{Level, debug, enabled, warn};

use crate::{config::BufferConfig, error::AudioError};

/// Largest piece we hand to the queue at once. Also caps how many permits a
/// single acquisition needs, so it can never exceed the whole budget.
const MAX_CHUNK_BYTES: usize = 32 * 1024;

/// Smallest window one ranged request will ask for.
///
/// Measured against googlevideo, every bounded range — 128 KiB through 2 MiB —
/// came back in the same ~40 ms, so window size buys nothing but fewer round
/// trips. Past this floor a *smaller* window is better: the window is what the
/// server holds open while the buffer drains, and a window far larger than the
/// buffer leaves a connection stalled for minutes waiting on the mixer.
const MIN_WINDOW_BYTES: u64 = 256 * 1024;

/// Stalls shorter than this are normal scheduling noise and not worth logging.
const STALL_LOG_THRESHOLD: Duration = Duration::from_millis(50);

/// How often the depth gauge samples the buffer, when debug logging is on.
const DEPTH_LOG_INTERVAL: Duration = Duration::from_secs(2);

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

        let window = window_bytes(budget_bytes);
        let (response, requested) = request_window(&spec, offset, window, &cancel).await?;

        // With a *windowed* request `Content-Length` describes the window, not
        // the file, so the total has to come from `Content-Range` or from
        // yt-dlp. The bare `Content-Length` is only meaningful as a last resort,
        // when the server ignored the range and sent the whole body.
        let len = total_from_content_range(response.headers())
            .or(spec.content_length)
            .or_else(|| {
                response
                    .content_length()
                    .map(|remaining| offset.saturating_add(remaining))
            });

        let budget = Arc::new(Semaphore::new(budget_bytes));
        let ready = Arc::new(Notify::new());

        // The semaphore is the real bound; the channel just needs enough slots
        // not to become the bottleneck.
        let channel_capacity = (budget_bytes / MAX_CHUNK_BYTES).max(4) + 4;
        let (tx, rx) = mpsc::channel(channel_capacity);

        let task = tokio::spawn(pump(
            spec.clone(),
            response,
            offset,
            requested,
            window,
            tx,
            Arc::clone(&budget),
            prefill_bytes,
            Arc::clone(&ready),
            cancel.clone(),
        ));

        // Sampling the semaphore costs an atomic load, but a task per stream is
        // not worth paying for when nobody is listening.
        if enabled!(Level::DEBUG) {
            tokio::spawn(log_depth(
                budget,
                budget_bytes,
                buffer.bytes_per_second(spec.bitrate_bps),
                cancel.clone(),
            ));
        }

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
///
/// `response` is the first window, already opened by [`PrefetchStream::open`]
/// so that a dead URL fails before songbird is handed a track; every window
/// after it is requested here.
#[allow(clippy::too_many_arguments)]
async fn pump(
    spec: SourceSpec,
    response: Response,
    offset: u64,
    requested: u64,
    window: u64,
    tx: mpsc::Sender<Result<Chunk, io::Error>>,
    budget: Arc<Semaphore>,
    prefill_bytes: u64,
    ready: Arc<Notify>,
    cancel: CancellationToken,
) {
    let mut next = Some((response, requested));
    let mut offset = offset;
    let mut buffered = 0u64;
    let mut announced = false;

    while let Some((response, requested)) = next.take() {
        let mut body = Box::pin(response.bytes_stream());
        let mut received = 0u64;

        let completed = loop {
            let next = tokio::select! {
                () = cancel.cancelled() => return,
                next = body.next() => next,
            };

            let mut bytes = match next {
                Some(Ok(bytes)) => bytes,
                Some(Err(error)) => {
                    let _ = tx.send(Err(io::Error::other(error))).await;

                    break false;
                }
                // The window is done; whether the file is depends on how much
                // of it arrived.
                None => break true,
            };

            received += bytes.len() as u64;

            while !bytes.is_empty() {
                let take = bytes.len().min(MAX_CHUNK_BYTES);
                let piece = bytes.split_to(take);

                // Acquiring permits for the whole piece is the backpressure:
                // once the configured buffer is full this parks until the mixer
                // catches up.
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
        };

        offset = offset.saturating_add(received);

        if !completed || at_end(&spec, offset, received, requested) {
            break;
        }

        debug!(offset, "requesting next window");

        next = match request_window(&spec, offset, window, &cancel).await {
            Ok(next) => Some(next),
            Err(AudioError::Cancelled) => return,
            // We asked past the last byte: the length we were working from was
            // a little short, and the file is simply over.
            Err(AudioError::HttpStatus(StatusCode::RANGE_NOT_SATISFIABLE)) => break,
            Err(error) => {
                let _ = tx.send(Err(io::Error::other(error))).await;

                break;
            }
        };
    }

    // Wake anyone still waiting to prefill — a short track can finish before
    // the target depth is ever reached.
    ready.notify_one();
}

/// Whether the window that just finished was the last one.
///
/// A window that came up short is the end of the file whatever any header
/// claimed, which is also how a source of unknown length terminates.
fn at_end(spec: &SourceSpec, offset: u64, received: u64, requested: u64) -> bool {
    received < requested || spec.content_length.is_some_and(|length| offset >= length)
}

/// How much one request asks for.
///
/// Sized from the buffer so that a window is drained in roughly one buffer's
/// worth of playback. That keeps the request bounded — which is all
/// googlevideo needs to serve it at link speed instead of pacing it — without
/// leaving a connection open and stalled long after the buffer has filled.
fn window_bytes(budget_bytes: usize) -> u64 {
    (budget_bytes as u64).max(MIN_WINDOW_BYTES)
}

/// Issue one ranged request, returning the response and how many bytes it
/// should carry.
async fn request_window(
    spec: &SourceSpec,
    offset: u64,
    window: u64,
    cancel: &CancellationToken,
) -> Result<(Response, u64), AudioError> {
    let (range, requested) = window_range(offset, spec.content_length, window);

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
    // whole body — which the pump copes with, since it counts what arrives.
    let status = response.status();
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

/// Log how full the buffer is, so `rust_music_bot=debug` shows depth over time.
///
/// Free permits are bytes the buffer has room for, so the difference is what it
/// is holding: everything queued plus the chunk the reader has in hand. Bytes
/// already handed to songbird's adapter are not counted — they are past us.
async fn log_depth(
    budget: Arc<Semaphore>,
    budget_bytes: usize,
    bytes_per_second: u64,
    cancel: CancellationToken,
) {
    let mut ticker = time::interval(DEPTH_LOG_INTERVAL);
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }

        let buffered = budget_bytes.saturating_sub(budget.available_permits());

        debug!(
            buffered_bytes = buffered,
            budget_bytes,
            buffered_ms = buffered as u64 * 1000 / bytes_per_second.max(1),
            "buffer depth"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

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
            assumed_bitrate_bps: 160_000,
        }
    }

    /// The window the test config produces, so the fixtures track the real
    /// sizing rather than a hard-coded constant.
    fn test_window() -> u64 {
        window_bytes(test_buffer_config().bytes_for(Some(160_000)))
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

    /// Serve ranged requests out of `body`, one connection per request, until
    /// the test finishes. Honours the range exactly, so what the client asks
    /// for is what it gets — and the counter makes the windowing observable.
    async fn serve_ranges(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = body.clone();
                let counter = Arc::clone(&counter);

                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut byte = [0u8; 1];
                    while let Ok(1) = socket.read(&mut byte).await {
                        request.push(byte[0]);

                        if request.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }

                    let Some((start, asked)) = parse_range(&String::from_utf8_lossy(&request))
                    else {
                        return;
                    };

                    counter.fetch_add(1, Ordering::SeqCst);

                    if start >= body.len() {
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;

                        return;
                    }

                    let end = asked.min(body.len() - 1);
                    let slice = &body[start..=end];

                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n",
                        slice.len(),
                        body.len()
                    );

                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }

                    let _ = socket.write_all(slice).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        (format!("http://{addr}/audio"), requests)
    }

    /// Pull the inclusive `bytes=<start>-<end>` pair out of a request head.
    fn parse_range(request: &str) -> Option<(usize, usize)> {
        let line = request
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("range:"))?;
        let (start, end) = line.split('=').nth(1)?.trim().split_once('-')?;

        Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
    }

    /// A body longer than one window arrives whole, in order, over several
    /// ranged requests.
    ///
    /// The request count is the point: a single request for the whole file is
    /// what googlevideo paces down to playback rate, so the windowing is the
    /// behaviour under test, not an implementation detail.
    #[tokio::test]
    async fn fetches_a_long_body_in_windows() {
        let len = test_window() as usize * 2 + 1024;
        let expected: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let (url, requests) = serve_ranges(expected.clone()).await;

        let cancel = CancellationToken::new();
        let spec = SourceSpec {
            client: Client::new(),
            url,
            headers: HeaderMap::new(),
            content_length: Some(len as u64),
            bitrate_bps: Some(160_000),
        };

        let mut stream = PrefetchStream::open(spec, test_buffer_config(), &cancel, 0)
            .await
            .expect("stream should open");

        let mut actual = Vec::new();
        tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut actual))
            .await
            .expect("read should not stall")
            .expect("read should succeed");

        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual, expected);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "two full windows plus the tail"
        );
    }

    /// The total comes from `Content-Range`, not from the window's own
    /// `Content-Length` — otherwise every long track would look 2 MiB long.
    #[tokio::test]
    async fn byte_len_reports_the_whole_file_not_the_window() {
        let len = test_window() as usize + 4096;
        let (url, _) = serve_ranges(vec![0u8; len]).await;

        let cancel = CancellationToken::new();
        let spec = SourceSpec {
            client: Client::new(),
            url,
            headers: HeaderMap::new(),
            // Deliberately absent, so `Content-Range` is the only source.
            content_length: None,
            bitrate_bps: Some(160_000),
        };

        let stream = PrefetchStream::open(spec, test_buffer_config(), &cancel, 0)
            .await
            .expect("stream should open");

        assert_eq!(stream.byte_len().await, Some(len as u64));
    }

    #[test]
    fn windows_are_bounded_and_clamped_to_the_file() {
        let window = MIN_WINDOW_BYTES;

        // Short file: one window covering all of it.
        assert_eq!(
            window_range(0, Some(1024), window),
            ("bytes=0-1023".into(), 1024)
        );
        assert_eq!(
            window_range(512, Some(1024), window),
            ("bytes=512-1023".into(), 512)
        );

        // Long file: a bounded window, never the whole thing.
        let (range, requested) = window_range(0, Some(window * 4), window);
        assert_eq!(range, format!("bytes=0-{}", window - 1));
        assert_eq!(requested, window);
    }

    #[test]
    fn windows_stay_bounded_without_a_length() {
        let window = MIN_WINDOW_BYTES;

        assert_eq!(
            window_range(0, None, window),
            (format!("bytes=0-{}", window - 1), window)
        );
        assert_eq!(
            window_range(64, Some(0), window),
            (format!("bytes=64-{}", 64 + window - 1), window)
        );
    }

    /// A window never falls below the floor, and grows with a deep buffer so a
    /// window is still drained in about one buffer's worth of playback.
    #[test]
    fn window_is_sized_from_the_buffer() {
        assert_eq!(window_bytes(1024), MIN_WINDOW_BYTES);
        assert_eq!(window_bytes(MIN_WINDOW_BYTES as usize), MIN_WINDOW_BYTES);
        assert_eq!(window_bytes(4 * 1024 * 1024), 4 * 1024 * 1024);
    }

    #[test]
    fn total_is_read_from_content_range() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, "bytes 0-1023/146800640".parse().unwrap());
        assert_eq!(total_from_content_range(&headers), Some(146_800_640));

        // An unknown total must not be mistaken for a real one.
        headers.insert(CONTENT_RANGE, "bytes 0-1023/*".parse().unwrap());
        assert_eq!(total_from_content_range(&headers), None);

        assert_eq!(total_from_content_range(&HeaderMap::new()), None);
    }

    /// A window that comes up short ends the stream, whatever the length said.
    #[test]
    fn a_short_window_ends_the_stream() {
        let spec = SourceSpec {
            client: Client::new(),
            url: String::new(),
            headers: HeaderMap::new(),
            content_length: Some(10_000),
            bitrate_bps: None,
        };

        assert!(at_end(&spec, 4_096, 4_096, 8_192), "server ran out early");
        assert!(!at_end(&spec, 8_192, 8_192, 8_192), "more file to come");
        assert!(at_end(&spec, 10_000, 1_808, 1_808), "reached the length");
    }
}
