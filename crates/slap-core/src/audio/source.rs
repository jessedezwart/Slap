//! The audio source that songbird uses.

use std::time::Instant;

use reqwest::Client;
use songbird::input::{AsyncAdapterStream, AudioStream, AudioStreamError, Compose, Input};
use symphonia_core::io::MediaSource;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use twilight_model::id::{Id, marker::GuildMarker};

use crate::{
    audio::{
        stream::{PrefetchStream, SourceSpec},
        ytdlp::{PlaybackTarget, Resolved, YtDlpService},
    },
    config::BufferConfig,
    error::AudioError,
};

/// Small buffer songbird needs. Value is based on the default buffer size in songbird's `AsyncAdapterStream`.
const ADAPTER_BUFFER_BYTES: usize = 64 * 1024;

pub struct OpusYtSource {
    guild_id: Option<Id<GuildMarker>>,
    target: PlaybackTarget,
    client: Client,
    ytdlp: YtDlpService,
    buffer_config: BufferConfig,
    /// Cancellation token, used for the complete guild.
    cancel: CancellationToken,
}

impl OpusYtSource {
    pub fn new(
        target: impl Into<PlaybackTarget>,
        client: Client,
        ytdlp: YtDlpService,
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
        }
    }

    /// Set the guild ID for this source.
    pub fn in_guild(mut self, guild_id: Id<GuildMarker>) -> Self {
        self.guild_id = Some(guild_id);
        self
    }

    async fn resolve(&mut self) -> Result<Resolved, AudioError> {
        // Start two async tasks:
        // - One that waits for the cancellation token to be triggered.
        // - One that runs yt-dlp to resolve the target.
        // Bias is used to ensure that if the cancellation token is triggered, we return early and don't wait for yt-dlp to finish.
        // This is done at the start of the poll and when the future is polled again after being woken up, so that we don't wait for yt-dlp to finish if the cancellation token is triggered while yt-dlp is running.
        let resolved: Resolved = tokio::select! {
            biased;
            () =self.cancel.cancelled() => return Err(AudioError::Cancelled),
            result = self.ytdlp.resolve(&self.target) => result?,
        };

        // Later re-resolves (expired URL, seek) must reach the same upload
        // rather than search again and maybe land on a different one.
        if let PlaybackTarget::Search(_) = self.target
            && let Some(url) = resolved.metadata.webpage_url.clone()
        {
            self.target = url.into();
        }

        Ok(resolved)
    }
}

#[async_trait::async_trait]
/// Everything here is async; songbird will use `create_async`.
impl Compose for OpusYtSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    // Don't log all arguments
    #[tracing::instrument(skip_all, fields(guild_id = self.guild_id.map(|id| id.get()), title = tracing::field::Empty))]
    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let resolved = self.resolve().await.map_err(fail)?;
        tracing::Span::current().record(
            "title",
            resolved
                .metadata
                .title
                .as_deref()
                .unwrap_or("Unknown track"),
        );

        let started = Instant::now();
        debug!(
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
        debug!(title = ?resolved.metadata.title, elapsed_ms = started.elapsed().as_millis(), "audio buffer ready");

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
}

/// Convert an `OpusYtSource` into a songbird `Input`. This is the type songbird expects to be returned from the `Compose` trait.
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
