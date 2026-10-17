//! yt-dlp: a client that runs the program, DTOs for the JSON it prints, and
//! the two roles built on top: a catalog that lists links and a resolver that
//! turns tracks into audio. Everything it hands out is a
//! [`crate::media`] type.
//!
//! yt-dlp is only ever used to *resolve* a video into a direct media URL plus
//! the headers needed to fetch it; the bytes themselves are pulled by
//! [`crate::audio::stream`]. The process is spawned as argv — there is no shell
//! anywhere in this path.

pub mod catalog;
pub mod client;
mod dto;
pub mod error;
pub mod resolver;

use url::Url;

pub use catalog::YtDlpCatalog;
pub use client::YtDlpClient;
pub use error::YtDlpError;
pub use resolver::YtDlpResolver;

use super::MediaError;
use crate::config::YtDlpConfig;

/// Limit imports so accidental channel/radio links cannot create an unbounded queue.
pub const PLAYLIST_LIMIT: usize = 1000;

/// yt-dlp plays two roles: listing links, and resolving tracks into audio.
/// Both run the same program, so they share one client.
pub fn build(config: YtDlpConfig) -> (YtDlpCatalog, YtDlpResolver) {
    let client = YtDlpClient::new(config);
    (
        YtDlpCatalog::new(client.clone()),
        YtDlpResolver::new(client),
    )
}

pub fn http_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;

    let scheme_ok = matches!(url.scheme(), "http" | "https");
    (scheme_ok && url.host_str().is_some()).then_some(url)
}

pub fn validate_url(raw: &str) -> Result<Url, MediaError> {
    http_url(raw).ok_or_else(|| MediaError::UnsupportedUrl(raw.to_owned()))
}

#[cfg(test)]
#[path = "../../tests/media/ytdlp/url.rs"]
mod tests;
