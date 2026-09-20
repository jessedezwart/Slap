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

                let Some((start, asked)) = parse_range(&String::from_utf8_lossy(&request)) else {
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
