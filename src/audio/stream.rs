//! The prefetch buffer that stands between the network and songbird's mixer.
//!
//! songbird's mixer thread reads a packet every 20 ms, synchronously. If that
//! read ever reaches the socket the thread blocks and *every* guild sharing
//! that mixer thread stutters. So a background task pulls bytes as fast as the
//! network allows into a byte-bounded queue, and the mixer only ever reads from
//! that queue.
//!
//! That background task and queue are songbird's own `AsyncAdapterStream`: it
//! pulls from our `AsyncRead` into a ring buffer sized to the configured
//! budget, and stops pulling while the ring is full. That is the backpressure
//! that stops a long track from being downloaded into memory all at once.
//!
//! "As fast as the network allows" needs one thing from the request shape:
//! googlevideo paces a request for a whole file down to roughly playback rate,
//! so a long track arrives in real time and the buffer can never rebuild after
//! a hiccup. A *bounded* range gets the fast path instead — measured on a 229
//! MiB track, one request for the lot ran at 36 KB/s while a 2 MiB window ran
//! at 45 MB/s — so the body is fetched as a series of ranged windows rather
//! than one open request. This is the same trick as yt-dlp's
//! `--http-chunk-size`.

mod range;
mod reader;
mod windows;

use reqwest::{Client, header::HeaderMap};

pub use reader::PrefetchStream;

/// Everything needed to open — or reopen — the byte stream.
#[derive(Clone)]
pub struct SourceSpec {
    pub client: Client,
    pub url: String,
    pub headers: HeaderMap,
    /// googlevideo wants an explicit upper bound (`bytes=0-1023`) rather than
    /// an open-ended range, so we pass yt-dlp's reported filesize through.
    pub content_length: Option<u64>,
    pub bitrate_bps: Option<u32>,
}
