mod range;
mod reader;
mod windows;

use reqwest::{Client, header::HeaderMap};

pub use reader::PrefetchStream;

use crate::media::StreamInfo;

#[derive(Clone)]
pub struct SourceSpec {
    // The shared HTTP client
    pub client: Client,
    // The URL to request from
    pub url: String,
    // The headers to send with the request
    pub headers: HeaderMap,
    /// Length of complete file. Optional since not always provided.
    pub content_length: Option<u64>,
    // Bitrate of the audio stream in bits per second. Falls back to the configured assumed bitrate when yt-dlp reports none.
    pub bitrate_bps: u32,
}

impl SourceSpec {
    /// `assumed_bitrate_bps` is used when the resolver reported no bitrate.
    pub fn new(client: Client, stream: &StreamInfo, assumed_bitrate_bps: u32) -> Self {
        Self {
            client,
            url: stream.media_url.clone(),
            headers: stream.headers.clone(),
            content_length: stream.content_length,
            bitrate_bps: stream.bitrate_bps.unwrap_or(assumed_bitrate_bps),
        }
    }
}
