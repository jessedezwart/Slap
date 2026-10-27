use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

use reqwest::{Client, header::HeaderMap};

use super::*;

/// A body comfortably larger than the smallest buffer we will allocate, so it
/// cannot arrive in the prefill alone.
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
    window_bytes(test_buffer_config().bytes_for(160_000))
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
        bitrate_bps: 160_000,
    };

    PrefetchStream::open(spec, test_buffer_config(), cancel, 0)
        .await
        .expect("stream should open")
}

/// Every byte arrives, in order, exactly once — the prefilled head followed
/// by the rest of the body.
#[tokio::test]
async fn streams_a_body_larger_than_the_buffer() {
    let expected = body();
    let (release, released) = oneshot::channel();
    let url = serve(expected.clone(), 40 * 1024, released).await;

    let cancel = CancellationToken::new();
    let mut stream = open_stream(url, &cancel).await;

    let _ = release.send(());

    let mut actual = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut actual))
        .await
        .expect("read should not stall")
        .expect("read should succeed");

    assert_eq!(actual.len(), expected.len());
    assert_eq!(actual, expected);
}

/// Opening returns once the prefill target is buffered, without waiting for
/// the rest of the body.
#[tokio::test]
async fn open_returns_before_the_body_is_complete() {
    let (release, released) = oneshot::channel();
    // 40 KiB covers the 32 KiB prefill target for a 64 KiB budget.
    let url = serve(body(), 40 * 1024, released).await;

    let cancel = CancellationToken::new();
    tokio::time::timeout(Duration::from_secs(5), open_stream(url, &cancel))
        .await
        .expect("open should complete without the rest of the body");

    let _ = release.send(());
}

/// Cancelling the guild token ends the stream even while the body is stalled.
#[tokio::test]
async fn cancelling_the_token_ends_the_stream() {
    let (release, released) = oneshot::channel();
    let url = serve(body(), 40 * 1024, released).await;

    let cancel = CancellationToken::new();
    let mut stream = open_stream(url, &cancel).await;

    // The guild is torn down while the body is still arriving.
    cancel.cancel();

    let mut actual = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut actual))
        .await
        .expect("stream should end promptly after cancellation")
        .expect("cancellation is a clean end");

    assert!(actual.len() < BODY_LEN);

    let _ = release.send(());
}

/// Serve ranged requests out of `body`, one connection per request, until
/// the test finishes. Honours the range exactly, so what the client asks
/// for is what it gets — and the counter makes the windowing observable.
async fn serve_ranges(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    serve_ranges_with(body, usize::MAX, true).await
}

async fn serve_ranges_with(
    body: Vec<u8>,
    max_response: usize,
    report_total: bool,
) -> (String, Arc<AtomicUsize>) {
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

                let end = asked
                    .min(body.len() - 1)
                    .min(start.saturating_add(max_response - 1));
                let slice = &body[start..=end];

                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n",
                    slice.len(),
                    if report_total {
                        body.len().to_string()
                    } else {
                        "*".into()
                    }
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
        bitrate_bps: 160_000,
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
        bitrate_bps: 160_000,
    };

    let stream = PrefetchStream::open(spec, test_buffer_config(), &cancel, 0)
        .await
        .expect("stream should open");

    assert_eq!(stream.byte_len().await, Some(len as u64));
}

/// Servers may cap each partial response below the requested window. Both
/// known and unknown totals must keep downloading from the next unread byte.
#[tokio::test]
async fn short_partial_responses_do_not_end_the_file() {
    for report_total in [true, false] {
        let expected = body();
        let cap = 128 * 1024;
        let (url, requests) = serve_ranges_with(expected.clone(), cap, report_total).await;
        let cancel = CancellationToken::new();
        let spec = SourceSpec {
            client: Client::new(),
            url,
            headers: HeaderMap::new(),
            content_length: None,
            bitrate_bps: 160_000,
        };
        let mut stream = PrefetchStream::open(spec, test_buffer_config(), &cancel, 0)
            .await
            .unwrap();
        assert_eq!(
            stream.byte_len().await,
            report_total.then_some(BODY_LEN as u64)
        );
        let mut actual = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut actual))
            .await
            .expect("download should finish")
            .expect("complete download");
        assert_eq!(actual, expected);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            BODY_LEN / cap + usize::from(!report_total)
        );
    }
}

/// HTTP's total must replace stale metadata for the download itself, not just
/// for byte_len(). Test both an undercount and an overcount.
#[tokio::test]
async fn http_total_overrides_metadata_for_all_windows() {
    for reported_size in [128 * 1024, BODY_LEN * 2] {
        let expected = body();
        let (url, requests) = serve_ranges(expected.clone()).await;
        let cancel = CancellationToken::new();
        let spec = SourceSpec {
            client: Client::new(),
            url,
            headers: HeaderMap::new(),
            content_length: Some(reported_size as u64),
            bitrate_bps: 160_000,
        };
        let mut stream = PrefetchStream::open(spec, test_buffer_config(), &cancel, 0)
            .await
            .unwrap();
        assert_eq!(stream.byte_len().await, Some(BODY_LEN as u64));
        let mut actual = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut actual))
            .await
            .expect("download should finish")
            .expect("complete download");
        assert_eq!(actual, expected);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            if reported_size < BODY_LEN { 3 } else { 2 }
        );
    }
}

/// A 416 is not a clean EOF when the known total still promises more bytes.
#[tokio::test]
async fn rejected_range_before_known_end_is_an_error() {
    let expected = body();
    let (url, _) = serve_ranges_with(expected.clone(), usize::MAX, false).await;
    let cancel = CancellationToken::new();
    let spec = SourceSpec {
        client: Client::new(),
        url,
        headers: HeaderMap::new(),
        content_length: Some(BODY_LEN as u64 * 2),
        bitrate_bps: 160_000,
    };
    let mut stream = PrefetchStream::open(spec, test_buffer_config(), &cancel, 0)
        .await
        .unwrap();
    let mut actual = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut actual))
        .await
        .expect("download should finish")
        .expect_err("missing bytes must be an error");
    assert_eq!(actual, expected);
}
