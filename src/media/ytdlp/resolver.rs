//! Turning a track into something playable: a media URL plus the headers
//! needed to fetch it.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use tracing::{debug, warn};
use url::Url;

use super::{YtDlpClient, parse};
use crate::media::{
    MediaError, Metadata, SearchQuery, StreamInfo, Target, search_cache::SearchCache,
};

#[derive(Clone)]
pub struct YtDlpResolver {
    client: YtDlpClient,
    /// Skips the search for tracks resolved on an earlier run.
    cache: Option<Arc<SearchCache>>,
}

impl YtDlpResolver {
    /// `client` is usually a clone of the one importing links.
    pub fn new(client: YtDlpClient, cache: Option<Arc<SearchCache>>) -> Self {
        Self { client, cache }
    }

    /// Resolve a link, or find and resolve the recording a search asks for.
    pub async fn resolve(&self, target: &Target) -> Result<StreamInfo, MediaError> {
        match target {
            Target::Url(url) => {
                debug!(host = url.host_str(), "resolving audio source");
                self.resolve_input(url.as_str()).await
            }
            Target::Search(query) => self.resolve_search(query).await,
        }
    }

    /// Resolve each source's top result until one runs about as long as the
    /// requested track.
    async fn resolve_search(&self, query: &SearchQuery) -> Result<StreamInfo, MediaError> {
        if let Some(url) = self.cache.as_ref().and_then(|c| c.get(&query.key)) {
            debug!(key = query.key, "reusing previously resolved upload");
            match self.resolve_input(&url).await {
                Ok(resolved) => return Ok(resolved),
                Err(error) if error.is_fatal() => return Err(error),
                // The upload can have been taken down since; search again.
                Err(error) => {
                    warn!(%error, "cached upload no longer resolves, searching again");
                }
            }
        }

        let text = query.text();
        for source in SearchSource::ORDER {
            debug!(query = ?text, source = source.name(), "searching for audio");
            let resolved = match self.resolve_input(&source.input(&text)).await {
                Ok(resolved) => resolved,
                // yt-dlp itself is unusable; the next source would fail the same way.
                Err(error) if error.is_fatal() => return Err(error),
                // No results, unavailable, no Opus: try the next source.
                Err(error) => {
                    warn!(source = source.name(), error = %error, "search failed");
                    continue;
                }
            };
            if durations_match(query, &resolved.metadata) {
                self.remember(query, &resolved).await;
                return Ok(resolved);
            }
            debug!(source = source.name(), title = ?resolved.metadata.title,
                duration_secs = resolved.metadata.duration.map(|d| d.as_secs()),
                "search result has the wrong duration");
        }
        warn!(query = ?text, "no search result matched");
        Err(MediaError::NoMatch(text))
    }

    /// Record which upload a track resolved to, so the next run skips the
    /// search entirely.
    async fn remember(&self, query: &SearchQuery, resolved: &StreamInfo) {
        let (Some(cache), Some(url)) = (self.cache.clone(), resolved.metadata.webpage_url.clone())
        else {
            return;
        };
        let url = url.to_string();
        let id = query.key.clone();
        // Committing fsyncs, so keep it off the runtime's worker threads.
        let _ = tokio::task::spawn_blocking(move || cache.put(&id, &url)).await;
    }

    /// Run `yt-dlp -j` and turn its JSON into a [`StreamInfo`].
    async fn resolve_input(&self, input: &str) -> Result<StreamInfo, MediaError> {
        let started = Instant::now();
        let output = self.client.video_json(input).await?;
        let resolved = parse::parse_stream(&output)?;
        debug!(title = ?resolved.metadata.title, duration_secs = resolved.metadata.duration.map(|d| d.as_secs()),
            elapsed_ms = started.elapsed().as_millis(), "audio source resolved");
        Ok(resolved)
    }
}

/// Decides whether a search result is the recording that was asked for.
/// Compares duration of requested track and ytdlp search result of the same track.
fn durations_match(query: &SearchQuery, found: &Metadata) -> bool {
    match (query.duration, found.duration) {
        // If both durations are known, check if they are within tolerance. When no duration is known, we assume it matches.
        (Some(wanted), Some(found)) => wanted.abs_diff(found) <= duration_tolerance(wanted),
        _ => true,
    }
}

/// Uploads vary a few seconds in padding (music video intros for example)
fn duration_tolerance(wanted: Duration) -> Duration {
    // Multiply by 0.07 to allow for a 7% difference in duration, but always allow at least 10 seconds of tolerance.
    Duration::from_secs(10).max(wanted.mul_f64(0.07))
}

/// Where searches look, best source first.
#[derive(Clone, Copy)]
pub(super) enum SearchSource {
    /// Label releases and official audio, without video intros.
    YoutubeMusic,
    /// Everything else: fan uploads, lyric videos, music videos.
    Youtube,
}

impl SearchSource {
    pub(super) const ORDER: [Self; 2] = [Self::YoutubeMusic, Self::Youtube];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::YoutubeMusic => "YouTube Music",
            Self::Youtube => "YouTube",
        }
    }

    /// The yt-dlp input; still one argv entry, so the text stays literal.
    pub(super) fn input(self, text: &str) -> String {
        match self {
            Self::YoutubeMusic => {
                let mut url =
                    Url::parse_with_params("https://music.youtube.com/search", [("q", text)])
                        .expect("static search URL is valid");
                // Songs only: videos are what the YouTube fallback is for.
                url.set_fragment(Some("songs"));
                url.into()
            }
            Self::Youtube => format!("ytsearch1:{text}"),
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/media/ytdlp/resolver.rs"]
mod tests;
