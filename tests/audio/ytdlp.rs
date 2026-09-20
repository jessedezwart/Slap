use super::*;

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

#[cfg(unix)]
#[tokio::test]
async fn search_prefers_youtube_music_and_falls_back_on_wrong_duration() {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!(
        "rust-music-bot-ytdlp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let log = path.with_extension("log");
    std::fs::write(&path, FAKE_YTDLP).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ytdlp = YtDlp::new(YtDlpConfig {
        program: path.to_string_lossy().into_owned(),
        timeout: Duration::from_secs(5),
        extra_args: Vec::new(),
    });
    let search = |title: &str| {
        PlaybackTarget::Search(SearchQuery {
            artists: vec!["Artist".into()],
            title: title.into(),
            duration: Some(Duration::from_secs(180)),
        })
    };
    let calls = || {
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        std::fs::remove_file(&log).ok();
        calls.lines().map(str::to_owned).collect::<Vec<_>>()
    };

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

/// Real searches where the top result has to be checked. Opt-in:
///   cargo test live_search -- --ignored --nocapture
#[tokio::test]
#[ignore = "requires yt-dlp and network access"]
async fn live_search_finds_the_requested_recording() {
    let ytdlp = YtDlp::new(YtDlpConfig {
        program: std::env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_owned()),
        timeout: Duration::from_secs(60),
        extra_args: Vec::new(),
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
        let query = SearchQuery {
            artists: artists.iter().map(|&a| a.to_owned()).collect(),
            title: title.into(),
            duration: Some(Duration::from_secs(secs)),
        };
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

#[test]
fn playlist_preserves_order_and_skips_invalid_or_unavailable_entries() {
    let json = br#"{"entries":[
            {"url":"https://example.com/first","title":"First","duration":123.5},
            null,
            {"url":"file:///secret"},
            {"url":"https://example.com/private","availability":"private"},
            {"webpage_url":"https://example.com/last","url":"https://cdn.example.com/audio","title":"Last","duration":-1}
        ]}"#;
    let list = parse_list(json, &Url::parse("https://example.com/playlist").unwrap()).unwrap();
    assert_eq!(list.skipped, 3);
    assert_eq!(list.tracks.len(), 2);
    assert_eq!(list.tracks[0].metadata.title.as_deref(), Some("First"));
    assert_eq!(
        list.tracks[0].metadata.duration,
        Some(Duration::from_millis(123500))
    );
    assert_eq!(
        match &list.tracks[1].target {
            PlaybackTarget::Url(url) => url.as_str(),
            _ => panic!("expected URL"),
        },
        "https://example.com/last"
    );
    assert_eq!(list.tracks[1].metadata.duration, None);
    assert!(list.single.is_none());
    assert!(!list.truncated);
}

#[test]
fn playlist_limit_and_empty_playlists() {
    let url = Url::parse("https://example.com/playlist").unwrap();
    let list = parse_list(br#"{"entries":[]}"#, &url).unwrap();
    assert!(list.tracks.is_empty());
    let entries: Vec<_> = (0..PLAYLIST_LIMIT + 1)
        .map(|i| serde_json::json!({"url":format!("https://example.com/{i}")}))
        .collect();
    let json = serde_json::to_vec(&serde_json::json!({"entries":entries})).unwrap();
    let list = parse_list(&json, &url).unwrap();
    assert!(list.truncated);
    assert_eq!(list.tracks.len(), PLAYLIST_LIMIT);
}

#[test]
fn single_track_listing_reuses_resolved_audio() {
    let url = Url::parse("https://example.com/watch").unwrap();
    let list = parse_list(
        br#"{"title":"Song","url":"https://cdn.example.com/audio","acodec":"opus"}"#,
        &url,
    )
    .unwrap();
    assert!(matches!(&list.tracks[0].target, PlaybackTarget::Url(actual) if actual == &url));
    assert_eq!(
        list.single.unwrap().media_url,
        "https://cdn.example.com/audio"
    );
}

#[test]
fn accepts_http_urls_from_any_host() {
    for url in [
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "https://youtu.be/dQw4w9WgXcQ",
        "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
        "https://example.com/watch?v=x",
        "http://example.com/audio.opus",
    ] {
        assert!(YtDlp::validate_url(url).is_ok(), "{url} should be accepted");
    }
}

#[test]
fn rejects_invalid_urls_and_other_schemes() {
    for url in [
        "file:///etc/passwd",
        "not a url",
        "ftp://example.com/audio.opus",
        "https://",
    ] {
        assert!(
            YtDlp::validate_url(url).is_err(),
            "{url} should be rejected"
        );
    }
}

/// The real shape of `yt-dlp -j`: one object on one line, trailing newline.
#[test]
fn parses_flattened_opus_format() {
    let json = br#"{"title":"Example","duration":212.0,"uploader":"Someone","webpage_url":"https://www.youtube.com/watch?v=x","url":"https://rr1---sn-x.googlevideo.com/videoplayback?x=1","acodec":"opus","abr":160.0,"filesize":4194304,"format_id":"251","http_headers":{"User-Agent":"Mozilla/5.0","Range":"bytes=0-"}}
"#;

    let resolved = parse_output(json).expect("should parse");

    assert_eq!(resolved.bitrate_bps, Some(160_000));
    assert_eq!(resolved.content_length, Some(4_194_304));
    assert_eq!(resolved.metadata.title.as_deref(), Some("Example"));
    assert_eq!(resolved.metadata.duration, Some(Duration::from_secs(212)));
    assert!(resolved.headers.contains_key("user-agent"));
    // Our own Range header must win.
    assert!(!resolved.headers.contains_key(RANGE));
}

/// Pretty-printed output still parses via the whole-buffer fallback.
#[test]
fn prefers_requested_downloads() {
    let json = br#"{
            "title": "Example",
            "url": "https://example.invalid/top-level",
            "acodec": "opus",
            "requested_downloads": [
                {"url": "https://example.invalid/chosen", "acodec": "opus", "abr": 128.0}
            ]
        }"#;

    let resolved = parse_output(json).expect("should parse");

    assert_eq!(resolved.media_url, "https://example.invalid/chosen");
    assert_eq!(resolved.bitrate_bps, Some(128_000));
}

#[test]
fn rejects_non_opus_audio() {
    let json =
        br#"{"title":"x","url":"https://example.invalid/a","acodec":"mp4a.40.2","abr":128.0}"#;

    assert!(matches!(parse_output(json), Err(AudioError::NoOpusFormat)));
}

#[test]
fn errors_on_empty_output() {
    assert!(matches!(
        parse_output(b"\n  \n"),
        Err(AudioError::NoMetadata)
    ));
}
