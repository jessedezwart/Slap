//! The audio source that songbird uses.

use std::{
    sync::Arc,
    time::{Duration},
};

use reqwest::Client;
use songbird::input::{AsyncAdapterStream, AudioStream, AudioStreamError, Compose, Input};
use symphonia_core::io::MediaSource;
use tokio_util::sync::CancellationToken;

use super::{
    AudioError,
    download::RemoteFile,
    stream::{HttpStream, budget_bytes},
};
use crate::media::{StreamInfo, Target, ytdlp::YtDlpResolver};

/// Bot-wide clients and settings.
pub struct AudioContext {
    /// Shared connection pool for every audio fetch.
    pub http: Client,
    /// Finds the media URL for a track once the queue reaches it.
    pub url_resolver: YtDlpResolver,
    /// How much audio to hold ahead of the mixer, in playback time.
    pub buffer: Duration,
}

pub struct TrackSource {
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
        cancel: CancellationToken,
    ) -> Self {
        Self {
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
        // Bias is used to ensure that if the cancellation token is triggered, no need to wait for yt-dlp to finish.
        // This is done at the start of the poll and when the future is polled again after being woken up, so that we don't wait for yt-dlp to finish if the cancellation token is triggered while yt-dlp is running.
        let resolved = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(AudioError::Cancelled),
            result = self.audio.url_resolver.resolve(&self.target) => result?,
        };

        // If the target was a search query, and we got a resolved URL, update the target to be that URL.
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
impl Compose for TrackSource {
    // This is never called, because async is set.
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    /// Called when songbird starts playing a track.
    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let resolved = self.resolve().await.map_err(fail)?;

        let file = RemoteFile::new(self.audio.http.clone(), &resolved);
        let budget_bytes = budget_bytes(self.audio.buffer, resolved.bitrate_bps);

        // Prefill buffer
        let stream = HttpStream::open(file, budget_bytes, &self.cancel, 0)
            .await
            .map_err(fail)?;

        Ok(AudioStream {
            input: Box::new(AsyncAdapterStream::new(Box::new(stream), budget_bytes))
                as Box<dyn MediaSource>,
        })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

/// Convert `TrackSource` into a songbird `Input`.
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
