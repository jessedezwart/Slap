use super::*;

#[test]
fn playlist_preserves_order_and_skips_invalid_or_unavailable_entries() {
    let json = br#"{"entries":[
            {"url":"https://example.com/first","title":"First","duration":123.5},
            null,
            {"url":"file:///secret"},
            {"url":"https://example.com/private","availability":"private"},
            {"webpage_url":"https://example.com/last","url":"https://cdn.example.com/audio","title":"Last","duration":-1}
        ]}"#;
    let (list, _) = parse_list(json, &Url::parse("https://example.com/playlist").unwrap()).unwrap();
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
    assert!(!list.truncated);
}

#[test]
fn playlist_limit_and_empty_playlists() {
    let url = Url::parse("https://example.com/playlist").unwrap();
    let (list, _) = parse_list(br#"{"entries":[]}"#, &url).unwrap();
    assert!(list.tracks.is_empty());
    let entries: Vec<_> = (0..PLAYLIST_LIMIT + 1)
        .map(|i| serde_json::json!({"url":format!("https://example.com/{i}")}))
        .collect();
    let json = serde_json::to_vec(&serde_json::json!({"entries":entries})).unwrap();
    let (list, _) = parse_list(&json, &url).unwrap();
    assert!(list.truncated);
    assert_eq!(list.tracks.len(), PLAYLIST_LIMIT);
}

#[test]
fn single_track_listing_yields_its_resolve() {
    let url = Url::parse("https://example.com/watch").unwrap();
    // Nothing else resolves this track: `list` puts the resolve straight into
    // the cache, and playback picks it up from there.
    let (list, resolved) = parse_list(
        br#"{"title":"Song","url":"https://cdn.example.com/audio","acodec":"opus"}"#,
        &url,
    )
    .unwrap();
    assert!(matches!(&list.tracks[0].target, PlaybackTarget::Url(actual) if actual == &url));
    assert_eq!(resolved.unwrap().media_url, "https://cdn.example.com/audio");
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
