//! The audio source that songbird uses.

use std::time::{Duration, Instant};

use reqwest::Client;
use songbird::input::{
    AsyncAdapterStream, AudioStream, AudioStreamError, AuxMetadata, Compose, Input,
};
use symphonia_core::io::MediaSource;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use twilight_model::id::{Id, marker::GuildMarker};

use crate::{
    audio::{
        stream::{PrefetchStream, SourceSpec},
        ytdlp::{PlaybackTarget, Resolved, YtDlp},
    },
    config::BufferConfig,
    error::AudioError,
};

/// Small buffer songbird needs. Value is based on the default buffer size in songbird's `AsyncAdapterStream`.
const ADAPTER_BUFFER_BYTES: usize = 64 * 1024;

/// How long a resolved media URL is reused before yt-dlp is run again.
const RESOLVE_TTL: Duration = Duration::from_secs(60);

pub struct OpusYtSource {
    guild_id: Option<Id<GuildMarker>>,
    target: PlaybackTarget,
    client: Client,
    ytdlp: YtDlp,
    buffer_config: BufferConfig,
    /// Cancellation token, used for the complete guild.
    cancel: CancellationToken,
    cached: Option<(Resolved, Instant)>,
}

impl OpusYtSource {
    pub fn new(
        target: impl Into<PlaybackTarget>,
        client: Client,
        ytdlp: YtDlp,
        buffer_config: BufferConfig,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            guild_id: None,
            target: target.into(),
            client,
            ytdlp,
            buffer_config,
            cancel,
            cached: None,
        }
    }

    /// Set the guild ID for this source.
    pub fn in_guild(mut self, guild_id: Id<GuildMarker>) -> Self {
        self.guild_id = Some(guild_id);
        self
    }

    /// Resolve the video, reusing a recent result if there is one.
    async fn resolve(&mut self) -> Result<Resolved, AudioError> {
        if let Some((resolved, at)) = &self.cached
            && at.elapsed() < RESOLVE_TTL
        {
            debug!(title = ?resolved.metadata.title, "reusing resolved audio metadata");
            return Ok(resolved.clone());
        }

        if self.cancel.is_cancelled() {
            return Err(AudioError::Cancelled);
        }

        let resolved = tokio::select! {
            () = self.cancel.cancelled() => return Err(AudioError::Cancelled),
            result = self.ytdlp.resolve(&self.target) => result?,
        };

        // Later re-resolves (expired URL, seek) must reach the same upload
        // rather than search again and maybe land on a different one.
        if let PlaybackTarget::Search(_) = self.target
            && let Some(url) = resolved
                .metadata
                .webpage_url
                .as_deref()
                .and_then(|url| YtDlp::validate_url(url).ok())
        {
            self.target = url.into();
        }

        self.cached = Some((resolved.clone(), Instant::now()));

        Ok(resolved)
    }

    /// Reuse the single-track metadata resolve performed while listing a URL.
    pub fn with_resolved(mut self, resolved: Resolved) -> Self {
        self.cached = Some((resolved, Instant::now()));
        self
    }
}

#[async_trait::async_trait]
impl Compose for OpusYtSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        // Everything here is async; songbird will use `create_async`.
        Err(AudioStreamError::Unsupported)
    }

    #[tracing::instrument(skip_all, fields(guild_id = self.guild_id.map(|id| id.get())))]
    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let resolved = self.resolve().await.map_err(fail)?;

        let started = Instant::now();
        info!(
            title = ?resolved.metadata.title,
            bitrate_bps = resolved.bitrate_bps,
            "opening audio stream"
        );

        let spec = SourceSpec {
            client: self.client.clone(),
            url: resolved.media_url,
            headers: resolved.headers,
            content_length: resolved.content_length,
            bitrate_bps: resolved.bitrate_bps,
        };

        let mut stream = PrefetchStream::open(spec, self.buffer_config, &self.cancel, 0)
            .await
            .map_err(fail)?;

        // Never hand songbird a track whose buffer is still empty.
        stream.prefill().await.map_err(fail)?;
        info!(title = ?resolved.metadata.title, elapsed_ms = started.elapsed().as_millis(), "audio buffer ready");

        Ok(AudioStream {
            input: Box::new(AsyncAdapterStream::new(
                Box::new(stream),
                ADAPTER_BUFFER_BYTES,
            )) as Box<dyn MediaSource>,
        })
    }

    fn should_create_async(&self) -> bool {
        true
    }

    async fn aux_metadata(&mut self) -> Result<AuxMetadata, AudioStreamError> {
        let resolved = self.resolve().await.map_err(fail)?;
        let metadata = resolved.metadata;

        Ok(AuxMetadata {
            title: metadata.title.clone(),
            track: metadata.title,
            artist: metadata.uploader.clone(),
            channel: metadata.uploader,
            duration: metadata.duration,
            source_url: metadata.webpage_url,
            thumbnail: metadata.thumbnail,
            ..AuxMetadata::default()
        })
    }
}

impl From<OpusYtSource> for Input {
    fn from(source: OpusYtSource) -> Self {
        Input::Lazy(Box::new(source))
    }
}

fn fail(error: AudioError) -> AudioStreamError {
    AudioStreamError::Fail(Box::new(error))
}

#[cfg(test)]
#[path = "../../tests/audio/source.rs"]
mod tests;
