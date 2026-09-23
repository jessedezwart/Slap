use std::time::Duration;

use super::*;
use crate::config::ResolveCacheConfig;

/// A Spotify track to search for. The ID only matters to the resolved-track
/// cache, which these tests leave disabled.
fn track(artists: &[&str], title: &str, secs: u64) -> SpotifyTrack {
    SpotifyTrack {
        id: format!("id-{title}"),
        artists: artists.iter().map(|&a| a.to_owned()).collect(),
        title: title.into(),
        duration: Some(Duration::from_secs(secs)),
        album_art: None,
        url: None,
    }
}

/// Fake yt-dlp: resolves each search input to its top result, and logs
/// every input so the tests can check which ones were run, in order.
#[cfg(unix)]
const FAKE_YTDLP: &str = r##"#!/bin/sh
before=; last=
for arg; do before=$last; last=$arg; done
# The input is always one literal argument after `--`.
[ "$before" = '--' ] || exit 7
[ "$1" = '-j' ] && [ "$6" = 'bestaudio[acodec=opus]' ] || exit 3
printf '%s\n' "$last" >> "$0.log"
case "$last" in
'https://music.youtube.com/search?q=Artist+Song+%24%28literal%29%3B#songs')
  printf '%s\n' '{"title":"Song $(literal);","duration":181,"artists":["Artist"],"uploader":"Artist - Topic","webpage_url":"https://www.youtube.com/watch?v=song","url":"https://cdn/song","acodec":"opus"}' ;;
'https://music.youtube.com/search?q=Artist+Rare#songs')
  printf '%s\n' '{"title":"Rare (Extended Mix)","duration":400,"url":"https://cdn/extended","acodec":"opus"}' ;;
'ytsearch1:Artist Rare')
  printf '%s\n' '{"title":"Artist - Rare (Official Audio)","duration":182,"uploader":"ArtistVEVO","url":"https://cdn/rare","acodec":"opus"}' ;;
*) echo 'ERROR: no results' >&2; exit 1 ;;
esac
"##;

/// Install `script` as a stand-in yt-dlp, returning it alongside a reader for
/// the inputs it has been run with since the last read.
#[cfg(unix)]
fn fake_ytdlp(script: &str) -> (YtDlpService, impl Fn() -> Vec<String>, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!(
        "slap-ytdlp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let log = path.with_extension("log");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ytdlp = YtDlpService::new(YtDlpConfig {
        program: path.to_string_lossy().into_owned(),
        timeout: Duration::from_secs(5),
        extra_args: Vec::new(),
        cookies_file: "secrets/cookies.txt".into(),
        resolve_cache: ResolveCacheConfig::default(),
    });
    let calls = move || {
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        std::fs::remove_file(&log).ok();
        calls.lines().map(str::to_owned).collect::<Vec<_>>()
    };
    (ytdlp, calls, path)
}

#[cfg(unix)]
#[tokio::test]
async fn search_prefers_youtube_music_and_falls_back_on_wrong_duration() {
    let (ytdlp, calls, path) = fake_ytdlp(FAKE_YTDLP);
    let search = |title: &str| PlaybackTarget::Search(track(&["Artist"], title, 180));

    // Shell syntax in a song name must reach yt-dlp as literal text.
    let found = ytdlp.resolve(&search("Song $(literal);")).await;
    let found_calls = calls();
    // YouTube Music's top result is an extended mix.
    let rare = ytdlp.resolve(&search("Rare")).await;
    let rare_calls = calls();
    let missing = ytdlp.resolve(&search("Missing")).await;
    let missing_calls = calls();
    std::fs::remove_file(&path).unwrap();

    let found = found.unwrap();
    assert_eq!(found.media_url, "https://cdn/song");
    assert_eq!(found.metadata.uploader.as_deref(), Some("Artist"));
    assert_eq!(
        found_calls,
        ["https://music.youtube.com/search?q=Artist+Song+%24%28literal%29%3B#songs"]
    );
    assert_eq!(rare.unwrap().media_url, "https://cdn/rare");
    assert_eq!(
        rare_calls,
        [
            "https://music.youtube.com/search?q=Artist+Rare#songs",
            "ytsearch1:Artist Rare",
        ]
    );
    assert!(matches!(missing, Err(AudioError::NoMatch(ref q)) if q == "Artist Missing"));
    assert_eq!(missing_calls.len(), 2);
}

/// Answers both the listing and the resolve invocation with the same single
/// video, logging every input it is handed.
#[cfg(unix)]
const FAKE_SINGLE_VIDEO: &str = r#"#!/bin/sh
last=
for arg; do last=$arg; done
printf '%s\n' "$last" >> "$0.log"
printf '%s\n' '{"title":"Song","duration":181,"webpage_url":"https://www.youtube.com/watch?v=song","url":"https://cdn/song","acodec":"opus"}'
"#;

/// Expanding a link already resolves it, so playing that link must not send
/// yt-dlp after the same video a second time.
#[cfg(unix)]
#[tokio::test]
async fn playing_a_single_link_runs_ytdlp_once() {
    let (ytdlp, calls, path) = fake_ytdlp(FAKE_SINGLE_VIDEO);
    let url = Url::parse("https://www.youtube.com/watch?v=song").unwrap();

    let list = ytdlp.list(&url).await.unwrap();
    assert_eq!(list.tracks.len(), 1);
    assert_eq!(calls(), [url.as_str()]);

    // What playback does when the track comes up.
    let resolved = ytdlp.resolve(&list.tracks[0].target).await.unwrap();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(resolved.media_url, "https://cdn/song");
    assert!(
        calls().is_empty(),
        "the listing's resolve should have been kept"
    );
}

/// Real searches where the top result has to be checked. Opt-in:
///   cargo test live_search -- --ignored --nocapture
#[tokio::test]
#[ignore = "requires yt-dlp and network access"]
async fn live_search_finds_the_requested_recording() {
    let ytdlp = YtDlpService::new(YtDlpConfig {
        program: std::env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_owned()),
        timeout: Duration::from_secs(60),
        extra_args: Vec::new(),
        cookies_file: "secrets/cookies.txt".into(),
        resolve_cache: ResolveCacheConfig::default(),
    });
    for (artists, title, secs) in [
        (
            &["Daft Punk", "Pharrell Williams", "Nile Rodgers"][..],
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
            369,
        ),
        (
            &["Daft Punk", "Pharrell Williams", "Nile Rodgers"][..],
            "Get Lucky - Radio Edit",
            248,
        ),
        (&["Beyoncé"][..], "Halo", 261),
        (&["Oasis"][..], "Live Forever - Remastered", 277),
    ] {
        let query = track(artists, title, secs);
        let started = Instant::now();
        let resolved = ytdlp
            .resolve(&PlaybackTarget::Search(query.clone()))
            .await
            .unwrap();
        println!(
            "{title}: {:?} by {:?}, {:?}, {:?} in {:?}",
            resolved.metadata.title,
            resolved.metadata.uploader,
            resolved.metadata.duration,
            resolved.metadata.webpage_url,
            started.elapsed()
        );
        assert!(matching::matches(&query, &resolved.metadata));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bot_challenge_stops_search_fallback_and_survives_shared_errors() {
    let (ytdlp, calls, path) = fake_ytdlp(
        r#"#!/bin/sh
printf '%s\n' called >> "$0.log"
echo "ERROR: [youtube] id: Sign in to confirm you’re not a bot. Use --cookies" >&2
exit 1
"#,
    );
    let error = ytdlp
        .resolve(&PlaybackTarget::Search(track(&["Artist"], "Song", 180)))
        .await
        .unwrap_err();
    assert!(error.cookies_required());
    assert!(error.is_fatal());
    assert_eq!(calls().len(), 1, "must not try another search source");
    let shared = AudioError::Shared(Arc::new(error));
    assert!(shared.cookies_required());
    assert!(shared.is_fatal());
    std::fs::remove_file(path).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn ignored_playlist_error_still_reports_cookie_requirement() {
    let (ytdlp, calls, path) = fake_ytdlp(
        r#"#!/bin/sh
printf '%s\n' called >> "$0.log"
echo "ERROR: [youtube] id: Sign in to confirm you're not a bot." >&2
printf '%s\n' '{"entries":[]}'
exit 0
"#,
    );
    let error = ytdlp
        .list(&Url::parse("https://youtube.com/playlist?list=test").unwrap())
        .await
        .unwrap_err();
    assert!(error.cookies_required());
    assert_eq!(calls().len(), 1);
    std::fs::remove_file(path).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn cookie_path_is_one_argument_for_listing_and_resolution() {
    let (mut ytdlp, calls, path) = fake_ytdlp(
        r#"#!/bin/sh
printf '%s\n' "$@" >> "$0.log"
printf '%s\n' '{"url":"https://cdn/audio","acodec":"opus"}'
"#,
    );
    ytdlp.client = YtDlpClient::new(YtDlpConfig {
        program: path.to_string_lossy().into_owned(),
        timeout: Duration::from_secs(5),
        extra_args: vec![],
        cookies_file: "/tmp/cookie directory/.env.cookies".into(),
        resolve_cache: ResolveCacheConfig::default(),
    });
    let url = Url::parse("https://youtube.com/watch?v=test").unwrap();
    ytdlp
        .resolve(&PlaybackTarget::Url(url.clone()))
        .await
        .unwrap();
    let resolve_args = calls();
    ytdlp.list(&url).await.unwrap();
    let list_args = calls();
    for args in [resolve_args, list_args] {
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--cookies", "/tmp/cookie directory/.env.cookies"])
        );
    }
    std::fs::remove_file(path).unwrap();
}
