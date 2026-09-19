//! The lazy audio source handed to songbird.
//!
//! This is a [`Compose`], so nothing happens until songbird actually needs the
//! track: a queued song spawns no yt-dlp process and opens no socket until the
//! queue's preloader readies it, a few seconds before it is due. Deep queues
//! therefore cost nothing, and a resolved googlevideo URL cannot go stale while
//! it waits its turn.

use std::time::{Duration, Instant};

use reqwest::Client;
use songbird::input::{
    AsyncAdapterStream, AudioStream, AudioStreamError, AuxMetadata, Compose, Input,
};
use symphonia_core::io::MediaSource;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{
    audio::{
        stream::{PrefetchStream, SourceSpec},
        ytdlp::{PlaybackTarget, Resolved, YtDlp},
    },
    config::BufferConfig,
    error::AudioError,
};

/// Ring buffer songbird uses to bridge our async stream to symphonia's
/// synchronous reads. Small on purpose: the deep buffer is ours.
const ADAPTER_BUFFER_BYTES: usize = 64 * 1024;

/// How long a resolved media URL is reused before yt-dlp is run again.
const RESOLVE_TTL: Duration = Duration::from_secs(60);

pub struct OpusYtSource {
    target: PlaybackTarget,
    client: Client,
    ytdlp: YtDlp,
    buffer: BufferConfig,
    /// The guild's token. Cancelling it tears down every stream this source
    /// has produced.
    cancel: CancellationToken,
    cached: Option<(Resolved, Instant)>,
}

impl OpusYtSource {
    pub fn new(
        target: impl Into<PlaybackTarget>,
        client: Client,
        ytdlp: YtDlp,
        buffer: BufferConfig,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            target: target.into(),
            client,
            ytdlp,
            buffer,
            cancel,
            cached: None,
        }
    }

    /// Resolve the video, reusing a recent result if there is one.
    async fn resolve(&mut self) -> Result<Resolved, AudioError> {
        if let Some((resolved, at)) = &self.cached
            && at.elapsed() < RESOLVE_TTL
        {
            return Ok(resolved.clone());
        }

        if self.cancel.is_cancelled() {
            return Err(AudioError::Cancelled);
        }

        let resolved = tokio::select! {
            () = self.cancel.cancelled() => return Err(AudioError::Cancelled),
            result = self.ytdlp.resolve(&self.target) => result?,
        };

        self.cached = Some((resolved.clone(), Instant::now()));

        Ok(resolved)
    }

    /// Reuse the single-track metadata resolve performed while listing a URL.
    pub fn with_resolved(mut self, resolved: Resolved) -> Self {
        self.cached = Some((resolved, Instant::now()));
        self
    }

    #[cfg(test)]
    pub async fn metadata(&mut self) -> Result<crate::audio::ytdlp::TrackMetadata, AudioError> {
        self.resolve().await.map(|resolved| resolved.metadata)
    }
}

#[async_trait::async_trait]
impl Compose for OpusYtSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        // Everything here is async; songbird will use `create_async`.
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let resolved = self.resolve().await.map_err(fail)?;

        debug!(
            title = resolved.metadata.title.as_deref().unwrap_or("?"),
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

        let mut stream = PrefetchStream::open(spec, self.buffer, &self.cancel, 0)
            .await
            .map_err(fail)?;

        // Never hand songbird a track whose buffer is still empty.
        stream.prefill().await.map_err(fail)?;

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
mod tests {
    use std::env;

    use songbird::input::codecs::{get_codec_registry, get_probe};
    use symphonia_core::codecs::CODEC_TYPE_OPUS;

    use super::*;
    use crate::config::YtDlpConfig;

    /// The whole design rests on songbird seeing an Opus track it can pass
    /// through untouched. This proves the chain end to end — yt-dlp resolve,
    /// ranged fetch, prefetch buffer, async adapter, WebM demux — and asserts
    /// the codec songbird ends up with, all without a Discord connection.
    ///
    /// Needs yt-dlp on PATH and network access, so it is opt-in:
    ///   cargo test -- --ignored --nocapture
    /// Override the video with `YTDLP_TEST_URL`.
    #[tokio::test]
    #[ignore = "requires yt-dlp and network access"]
    async fn youtube_audio_parses_as_opus() {
        let raw_url = env::var("YTDLP_TEST_URL")
            .unwrap_or_else(|_| "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned());

        let url = YtDlp::validate_url(&raw_url).expect("test URL should be a YouTube link");

        let ytdlp = YtDlp::new(YtDlpConfig {
            program: env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_owned()),
            timeout: Duration::from_secs(60),
            extra_args: Vec::new(),
        });

        let buffer = crate::config::BufferConfig {
            target: Duration::from_millis(5_000),
            prefill_ratio: 0.5,
            assumed_bitrate_bps: 160_000,
        };

        let cancel = CancellationToken::new();
        let mut source =
            OpusYtSource::new(url, reqwest::Client::new(), ytdlp, buffer, cancel.clone());

        let metadata = source.metadata().await.expect("yt-dlp should resolve");
        println!("resolved: {:?}", metadata.title);

        let input: Input = source.into();
        let input = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("input should parse");

        let Input::Live(live, _) = input else {
            panic!("input should be live after make_playable_async");
        };

        let parsed = live.parsed().expect("stream should be parsed");
        let track = parsed
            .format
            .default_track()
            .or_else(|| parsed.format.tracks().first())
            .expect("container should expose a track");

        println!(
            "container track codec: {:?}, rate: {:?}",
            track.codec_params.codec, track.codec_params.sample_rate
        );

        // This is the passthrough precondition: Opus in, Opus out, no transcode.
        assert_eq!(
            track.codec_params.codec, CODEC_TYPE_OPUS,
            "songbird must see an Opus track or it would decode and re-encode"
        );

        cancel.cancel();
    }
}
