use std::{env, time::Duration};

use songbird::input::codecs::{get_codec_registry, get_probe};
use symphonia_core::codecs::CODEC_TYPE_OPUS;

use super::*;
use crate::config::{ResolveCacheConfig, YtDlpConfig};

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
        resolve_cache: ResolveCacheConfig::default(),
    });

    let buffer = crate::config::BufferConfig {
        target: Duration::from_millis(5_000),
        prefill_ratio: 0.5,
        assumed_bitrate_bps: 160_000,
    };

    let cancel = CancellationToken::new();
    let mut source = OpusYtSource::new(url, reqwest::Client::new(), ytdlp, buffer, cancel.clone());

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
