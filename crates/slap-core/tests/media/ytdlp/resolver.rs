use std::time::Duration;

use super::*;
#[cfg(unix)]
use crate::media::{catalog::Catalog, ytdlp::YtDlpCatalog};
use crate::{config::YtDlpConfig, media::ytdlp::build};

/// A recording to search for. The key only matters to the resolved-track
/// cache, which these tests leave disabled.
fn track(artists: &[&str], title: &str, secs: u64) -> SearchQuery {
    SearchQuery {
        key: format!("id-{title}"),
        artist: artists.join(", "),
        title: title.into(),
        duration: Some(Duration::from_secs(secs)),
    }
}

/// yt-dlp settings for running `program`.
#[cfg(unix)]
fn config(program: &std::path::Path) -> YtDlpConfig {
    YtDlpConfig {
        program: program.to_string_lossy().into_owned(),
        timeout: Duration::from_secs(5),
        extra_args: Vec::new(),
        cookies_file: "secrets/cookies.txt".into(),
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
fn fake_ytdlp(
    script: &str,
) -> (
    YtDlpCatalog,
    YtDlpResolver,
    impl Fn() -> Vec<String>,
    std::path::PathBuf,
) {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!(
        "slap-ytdlp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let log = path.with_extension("log");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (catalog, resolver) = build(config(&path));
    let calls = move || {
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        std::fs::remove_file(&log).ok();
        calls.lines().map(str::to_owned).collect::<Vec<_>>()
    };
    (catalog, resolver, calls, path)
}

#[cfg(unix)]
#[tokio::test]
async fn search_prefers_youtube_music_and_falls_back_on_wrong_duration() {
    let (_, ytdlp, calls, path) = fake_ytdlp(FAKE_YTDLP);
    let search = |title: &str| Target::Search(track(&["Artist"], title, 180));

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
    assert!(matches!(missing, Err(MediaError::NoMatch(ref q)) if q == "Artist Missing"));
    assert_eq!(missing_calls.len(), 2);
}

/// Answers the listing with a single video, logging every input it is handed.
#[cfg(unix)]
const FAKE_SINGLE_VIDEO: &str = r#"#!/bin/sh
last=
for arg; do last=$arg; done
printf '%s\n' "$last" >> "$0.log"
printf '%s\n' '{"title":"Song","duration":181,"webpage_url":"https://www.youtube.com/watch?v=song","url":"https://cdn/song","acodec":"opus"}'
"#;

/// Listing a single video already yields its stream, so the track is ready
/// to play after one yt-dlp run.
#[cfg(unix)]
#[tokio::test]
async fn listing_a_single_link_carries_its_stream() {
    let (catalog, _, calls, path) = fake_ytdlp(FAKE_SINGLE_VIDEO);
    let url = Url::parse("https://www.youtube.com/watch?v=song").unwrap();

    let import = catalog.list(&url).await.unwrap();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(import.tracks.len(), 1);
    let stream = import.tracks[0]
        .stream
        .as_ref()
        .expect("the listing resolved it");
    assert_eq!(stream.media_url, "https://cdn/song");
    assert_eq!(calls(), [url.as_str()]);
}

/// Real searches where the top result has to be checked. Opt-in:
///   cargo test live_search -- --ignored --nocapture
#[tokio::test]
#[ignore = "requires yt-dlp and network access"]
async fn live_search_finds_the_requested_recording() {
    let (_, ytdlp) = build(YtDlpConfig {
        program: std::env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_owned()),
        timeout: Duration::from_secs(60),
        extra_args: Vec::new(),
        cookies_file: "secrets/cookies.txt".into(),
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
        let resolved = ytdlp.resolve(&Target::Search(query.clone())).await.unwrap();
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
async fn cookie_path_is_one_argument_for_listing_and_resolution() {
    let (_, _, calls, path) = fake_ytdlp(
        r#"#!/bin/sh
printf '%s\n' "$@" >> "$0.log"
printf '%s\n' '{"url":"https://cdn/audio","acodec":"opus"}'
"#,
    );
    // Only an existing cookie file is passed on.
    let cookie_dir = path.with_extension("cookie directory");
    std::fs::create_dir_all(&cookie_dir).unwrap();
    let cookie_file = cookie_dir.join(".env.cookies");
    std::fs::write(&cookie_file, "").unwrap();
    let cookie_arg = cookie_file.to_string_lossy().into_owned();
    let (catalog, resolver) = build(YtDlpConfig {
        cookies_file: cookie_file.clone(),
        ..config(&path)
    });
    let url = Url::parse("https://youtube.com/watch?v=test").unwrap();
    resolver.resolve(&Target::Url(url.clone())).await.unwrap();
    let resolve_args = calls();
    catalog.list(&url).await.unwrap();
    let list_args = calls();
    for args in [resolve_args, list_args] {
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--cookies", cookie_arg.as_str()])
        );
    }
    std::fs::remove_dir_all(cookie_dir).unwrap();
    std::fs::remove_file(path).unwrap();
}
