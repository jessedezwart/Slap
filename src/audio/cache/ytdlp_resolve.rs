//! Caches signed media URLs resolved by yt-dlp. They expire after a few minutes.

use std::{
    future::Future,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use moka::{Expiry, future::Cache};
use tracing::debug;
use url::Url;

use crate::{audio::ytdlp::Resolved, config::ResolveCacheConfig, error::AudioError};

const EXPIRY_MARGIN: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct YtdlpResolveCache {
    entries: Cache<String, Resolved>,
}

impl YtdlpResolveCache {
    pub fn new(config: ResolveCacheConfig) -> Self {
        Self {
            entries: Cache::builder()
                .max_capacity(config.entries)
                .expire_after(MediaUrlExpiry {
                    max_ttl: config.max_ttl,
                })
                .build(),
        }
    }

    pub async fn insert(&self, input: &str, resolved: Resolved) {
        debug!(input, title = ?resolved.metadata.title, "keeping a resolve made while listing");
        self.entries.insert(input.to_owned(), resolved).await;
    }

    pub async fn get_or_resolve<F>(&self, input: &str, resolve: F) -> Result<Resolved, AudioError>
    where
        F: Future<Output = Result<Resolved, AudioError>>,
    {
        let entry = self
            .entries
            .entry_by_ref(input)
            .or_try_insert_with(resolve)
            .await
            .map_err(AudioError::shared)?;

        if !entry.is_fresh() {
            debug!(input, title = ?entry.value().metadata.title, "reusing resolved audio source");
        }

        Ok(entry.into_value())
    }

    #[cfg(test)]
    async fn len(&self) -> u64 {
        self.entries.run_pending_tasks().await;
        self.entries.entry_count()
    }
}

/// Keeps an entry only as long as the media URL it holds is usable.
struct MediaUrlExpiry {
    max_ttl: Duration,
}

impl Expiry<String, Resolved> for MediaUrlExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Resolved,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(get_signed_url_ttl(&value.media_url).map_or(self.max_ttl, |ttl| ttl.min(self.max_ttl)))
    }
}

fn get_signed_url_ttl(media_url: &str) -> Option<Duration> {
    let url = Url::parse(media_url).ok()?;

    // `videoplayback?...&expire=1758...`, or the path-segment form the same
    // URLs sometimes take: `/expire/1758.../`.
    let expires_at: u64 = url
        .query_pairs()
        .find(|(name, _)| name == "expire")
        .and_then(|(_, value)| value.parse().ok())
        .or_else(|| {
            let mut segments = url.path_segments()?;
            segments.find(|s| *s == "expire")?;
            segments.next()?.parse().ok()
        })?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();

    // saturating_sub to avoid underflow.
    // EXPIRY_MARGIN to avoid the cache expiring just as the URL does.
    Some(Duration::from_secs(expires_at.saturating_sub(now)).saturating_sub(EXPIRY_MARGIN))
}

#[cfg(test)]
#[path = "../../../tests/audio/cache/ytdlp_resolve.rs"]
mod tests;
