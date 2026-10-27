//! The audio source that songbird uses.

use std::{sync::Arc, time::Instant};

use reqwest::Client;
use songbird::input::{AsyncAdapterStream, AudioStream, AudioStreamError, Compose, Input};
use symphonia_core::io::MediaSource;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use twilight_model::id::{Id, marker::GuildMarker};

use super::{AudioError, download::RemoteFile, stream::HttpStream};
use crate::{
    config::BufferConfig,
    media::{StreamInfo, Target, ytdlp::YtDlpResolver},
};

/// Bot-wide clients and settings.
pub struct AudioContext {
    /// Shared connection pool for every audio fetch.
    pub http: Client,
    /// Turns a track into audio once the queue reaches it.
    pub resolver: YtDlpResolver,
    pub buffer: BufferConfig,
}

pub struct TrackSource {
    guild_id: Id<GuildMarker>,
    target: Target,
    stream: Option<StreamInfo>,
    audio: Arc<AudioContext>,
    /// Cancellation token, used for the complete guild.
    cancel: CancellationToken,
}

impl TrackSource {
    pub fn new(
        target: impl Into<Target>,
        stream: Option<StreamInfo>,
        audio: Arc<AudioContext>,
        guild_id: Id<GuildMarker>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            guild_id,
            target: target.into(),
            stream,
            audio,
            cancel,
        }
    }

    async fn resolve(&mut self) -> Result<StreamInfo, AudioError> {
        // songbird reopens the source on a seek; reuse what still works.
        if let Some(stream) = self.stream.as_ref().filter(|stream| stream.is_fresh()) {
            return Ok(stream.clone());
        }

        // Start two async tasks:
        // - One that waits for the cancellation token to be triggered.
        // - One that runs yt-dlp to resolve the target.
        // Bias is used to ensure that if the cancellation token is triggered, we return early and don't wait for yt-dlp to finish.
        // This is done at the start of the poll and when the future is polled again after being woken up, so that we don't wait for yt-dlp to finish if the cancellation token is triggered while yt-dlp is running.
        let resolved = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(AudioError::Cancelled),
            result = self.audio.resolver.resolve(&self.target) => result?,
        };

        // Later re-resolves (expired URL, seek) must reach the same upload
        // rather than search again and maybe land on a different one.
        if let Target::Search(_) = self.target
            && let Some(url) = resolved.metadata.webpage_url.clone()
        {
            self.target = url.into();
        }

        self.stream = Some(resolved.clone());
        Ok(resolved)
    }
}

#[async_trait::async_trait]
/// Everything here is async; songbird will use `create_async`.
impl Compose for TrackSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    // Don't log all arguments
    #[tracing::instrument(skip_all, fields(guild_id = self.guild_id.get(), title = tracing::field::Empty))]
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

        let file = RemoteFile::new(self.audio.http.clone(), &resolved);

        // Returns once the prefill is buffered.
        let stream = HttpStream::open(file, self.audio.buffer, &self.cancel, 0)
            .await
            .map_err(fail)?;
        debug!(title = ?resolved.metadata.title, elapsed_ms = started.elapsed().as_millis(), "audio buffer ready");

        // songbird's adapter does the read-ahead, so its ring is the buffer.
        let budget_bytes = stream.budget_bytes();
        Ok(AudioStream {
            input: Box::new(AsyncAdapterStream::new(Box::new(stream), budget_bytes))
                as Box<dyn MediaSource>,
        })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

/// Convert a `TrackSource` into a songbird `Input`. This is the type songbird expects to be returned from the `Compose` trait.
impl From<TrackSource> for Input {
    fn from(source: TrackSource) -> Self {
        Input::Lazy(Box::new(source))
    }
}

fn fail(error: AudioError) -> AudioStreamError {
    AudioStreamError::Fail(Box::new(error))
}

#[cfg(test)]
#[path = "../../tests/audio/source.rs"]
mod tests;
