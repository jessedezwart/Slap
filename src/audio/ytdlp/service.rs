//! Turning what a user asked for into something playable.

use std::{sync::Arc, time::Instant};

use tracing::{info, warn};
use url::Url;

use super::{
    PlaybackTarget, YtDlpClient,
    models::{self, MediaList, Resolved},
};
use crate::{
    audio::{
        cache::{SpotifyTrackCache, YtdlpResolveCache},
        matching,
    },
    config::YtDlpConfig,
    error::AudioError,
    spotify::SpotifyTrack,
};

#[derive(Clone)]
pub struct YtDlpService {
    pub(super) client: YtDlpClient,
    /// Skips the search for tracks resolved on an earlier run.
    cache: Option<Arc<SpotifyTrackCache>>,
    /// Skips yt-dlp entirely for an input another guild just resolved.
    resolve_cache: YtdlpResolveCache,
}

impl YtDlpService {
    pub fn new(config: YtDlpConfig) -> Self {
        Self {
            resolve_cache: YtdlpResolveCache::new(config.resolve_cache),
            client: YtDlpClient::new(config),
            cache: None,
        }
    }

    pub fn with_track_cache(mut self, cache: Option<Arc<SpotifyTrackCache>>) -> Self {
        self.cache = cache;
        self
    }

    /// Resolve a link, or find and resolve the recording a search asks for.
    pub async fn resolve(&self, target: &PlaybackTarget) -> Result<Resolved, AudioError> {
        match target {
            PlaybackTarget::Url(url) => {
                info!(host = url.host_str(), "resolving audio source");
                self.resolve_input(url.as_str()).await
            }
            PlaybackTarget::Search(wanted) => self.resolve_search(wanted).await,
        }
    }

    /// Resolve each source's top result until one runs about as long as the
    /// requested track.
    async fn resolve_search(&self, wanted: &SpotifyTrack) -> Result<Resolved, AudioError> {
        if let Some(url) = self.cache.as_ref().and_then(|c| c.get(&wanted.id)) {
            info!(spotify_id = wanted.id, "reusing previously resolved upload");
            match self.resolve_input(&url).await {
                Ok(resolved) => return Ok(resolved),
                // The upload can have been taken down since; search again.
                Err(error) => {
                    warn!(%error, "cached upload no longer resolves, searching again");
                }
            }
        }

        let text = format!("{} {}", wanted.artist(), wanted.title);
        for source in SearchSource::ORDER {
            info!(query = ?text, source = source.name(), "searching for audio");
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
            if matching::matches(wanted, &resolved.metadata) {
                self.remember(wanted, &resolved).await;
                return Ok(resolved);
            }
            info!(source = source.name(), title = ?resolved.metadata.title,
                duration_secs = resolved.metadata.duration.map(|d| d.as_secs()),
                "search result has the wrong duration");
        }
        warn!(query = ?text, "no search result matched");
        Err(AudioError::NoMatch(text))
    }

    /// Record which upload a track resolved to, so the next run skips the
    /// search entirely.
    async fn remember(&self, wanted: &SpotifyTrack, resolved: &Resolved) {
        let (Some(cache), Some(url)) = (self.cache.clone(), resolved.metadata.webpage_url.clone())
        else {
            return;
        };
        let url = url.to_string();
        let id = wanted.id.clone();
        // Committing fsyncs, so keep it off the runtime's worker threads.
        let _ = tokio::task::spawn_blocking(move || cache.put(&id, &url)).await;
    }

    /// Resolve an input, reusing the result another caller just got for it.
    async fn resolve_input(&self, input: &str) -> Result<Resolved, AudioError> {
        self.resolve_cache
            .get_or_resolve(input, self.run_resolve(input))
            .await
    }

    /// Run `yt-dlp -j` and turn its JSON into a [`Resolved`].
    async fn run_resolve(&self, input: &str) -> Result<Resolved, AudioError> {
        let started = Instant::now();
        let output = self.client.resolve(input).await?;
        let resolved = models::parse_output(&output)?;
        info!(title = ?resolved.metadata.title, duration_secs = resolved.metadata.duration.map(|d| d.as_secs()),
            elapsed_ms = started.elapsed().as_millis(), "audio source resolved");
        Ok(resolved)
    }

    /// Expand playlists without resolving each entry's expiring audio URL.
    pub async fn list(&self, url: &Url) -> Result<MediaList, AudioError> {
        let output = self.client.list(url).await?;
        let (list, resolved) = models::parse_list(&output, url)?;

        // A lone video is fully resolved by the listing itself. Keeping that
        // resolve is what stops playback from running yt-dlp over again.
        if let Some(resolved) = resolved {
            self.resolve_cache.insert(url.as_str(), resolved).await;
        }

        Ok(list)
    }
}

/// Where searches look, best source first.
#[derive(Clone, Copy)]
enum SearchSource {
    /// Label releases and official audio, without video intros.
    YoutubeMusic,
    /// Everything else: fan uploads, lyric videos, music videos.
    Youtube,
}

impl SearchSource {
    const ORDER: [Self; 2] = [Self::YoutubeMusic, Self::Youtube];

    fn name(self) -> &'static str {
        match self {
            Self::YoutubeMusic => "YouTube Music",
            Self::Youtube => "YouTube",
        }
    }

    /// The yt-dlp input; still one argv entry, so the text stays literal.
    fn input(self, text: &str) -> String {
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
#[path = "../../../tests/audio/ytdlp/service.rs"]
mod tests;
