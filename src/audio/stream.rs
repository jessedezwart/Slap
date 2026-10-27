//! The prefetch buffer that stands between the network and songbird's mixer.
//!
//! songbird's mixer thread reads a packet every 20 ms, synchronously. If that
//! read ever reaches the socket the thread blocks and *every* guild sharing
//! that mixer thread stutters. So a background task pulls bytes as fast as the
//! network allows into a byte-bounded queue, and the mixer only ever reads from
//! that queue.
//!
//! The bound is a [`Semaphore`](tokio::sync::Semaphore) holding one permit per buffered byte, so the
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

mod pump;
mod range;
mod reader;

use bytes::Bytes;
use reqwest::{Client, header::HeaderMap};
use tokio::sync::OwnedSemaphorePermit;

pub use reader::PrefetchStream;

/// Largest piece we hand to the queue at once. Also caps how many permits a
/// single acquisition needs, so it can never exceed the whole budget.
const MAX_CHUNK_BYTES: usize = 32 * 1024;

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

/// A buffered chunk. Dropping it returns its bytes to the budget.
struct Chunk {
    bytes: Bytes,
    _permit: OwnedSemaphorePermit,
}
