use std::{env, time::Duration};

use songbird::input::codecs::{get_codec_registry, get_probe};
use symphonia_core::codecs::CODEC_TYPE_OPUS;

use super::*;
use crate::{
    config::YtDlpConfig,
    media::{Metadata, ytdlp::YtDlpClient},
};

/// A resolver running yt-dlp as `config` says, without the search cache.
fn resolver(config: YtDlpConfig) -> YtDlpResolver {
    YtDlpResolver::new(YtDlpClient::new(config), None)
}

/// The shared context around `resolver`, with a 5 s buffer.
fn audio(resolver: YtDlpResolver) -> Arc<AudioContext> {
    Arc::new(AudioContext {
        http: reqwest::Client::new(),
        resolver,
        buffer: Duration::from_millis(5_000),
    })
}

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

    let url = crate::media::validate_url(&raw_url).expect("test URL should be a YouTube link");

    let resolver = resolver(YtDlpConfig {
        program: env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_owned()),
        timeout: Duration::from_secs(60),
        extra_args: Vec::new(),
        cookies_file: "secrets/cookies.txt".into(),
    });

    let cancel = CancellationToken::new();
    let mut source = TrackSource::new(url, None, audio(resolver), cancel.clone());

    let metadata = source
        .resolve()
        .await
        .expect("yt-dlp should resolve")
        .metadata;
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

/// A source whose resolver cannot run: any attempt to resolve fails to spawn.
fn source_without_ytdlp(stream: Option<StreamInfo>) -> TrackSource {
    let resolver = resolver(YtDlpConfig {
        program: "slap-test-no-such-program".to_owned(),
        timeout: Duration::from_secs(5),
        extra_args: Vec::new(),
        cookies_file: "secrets/cookies.txt".into(),
    });
    let url: url::Url = "https://www.youtube.com/watch?v=song".parse().unwrap();
    TrackSource::new(
        url,
        stream,
        audio(resolver),
        CancellationToken::new(),
    )
}

fn listed_stream(expires_at: Option<std::time::SystemTime>) -> StreamInfo {
    StreamInfo {
        media_url: "https://cdn/song".to_owned(),
        headers: reqwest::header::HeaderMap::new(),
        content_length: None,
        bitrate_bps: None,
        expires_at,
        metadata: Metadata::default(),
    }
}

#[tokio::test]
async fn a_listed_stream_plays_without_the_resolver() {
    let mut source = source_without_ytdlp(Some(listed_stream(None)));

    let stream = source.resolve().await.expect("the listed stream is used");
    assert_eq!(stream.media_url, "https://cdn/song");

    // Reopening the source (a seek) reuses it while it still works.
    let again = source.resolve().await.expect("the stream is kept");
    assert_eq!(again.media_url, "https://cdn/song");
}

#[tokio::test]
async fn an_expired_listed_stream_is_resolved_again() {
    let expired = std::time::SystemTime::now() - Duration::from_secs(60);
    let mut source = source_without_ytdlp(Some(listed_stream(Some(expired))));

    // The resolver was asked, and it cannot run here.
    assert!(source.resolve().await.is_err());
}
