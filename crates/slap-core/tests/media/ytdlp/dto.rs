use super::*;
use crate::media::Target;

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
            Target::Url(url) => url.as_str(),
            _ => panic!("expected URL"),
        },
        "https://example.com/last"
    );
    assert_eq!(list.tracks[1].metadata.duration, None);
    // Flat entries leave the stream to the resolver.
    assert!(list.tracks.iter().all(|track| track.stream.is_none()));
    assert!(!list.truncated);
}

#[test]
fn playlist_limit_and_empty_playlists() {
    let url = Url::parse("https://example.com/playlist").unwrap();
    let list = parse_list(br#"{"entries":[]}"#, &url).unwrap();
    assert!(list.tracks.is_empty());
    assert_eq!(list.playlist_name, None);
    let list = parse_list(br#"{"title":"  Road Trip  ","entries":[]}"#, &url).unwrap();
    assert_eq!(list.playlist_name.as_deref(), Some("Road Trip"));
    let list = parse_list(br#"{"title":"   ","entries":[]}"#, &url).unwrap();
    assert_eq!(list.playlist_name, None);
    let entries: Vec<_> = (0..PLAYLIST_LIMIT + 1)
        .map(|i| serde_json::json!({"url":format!("https://example.com/{i}")}))
        .collect();
    let json = serde_json::to_vec(&serde_json::json!({"entries":entries})).unwrap();
    let list = parse_list(&json, &url).unwrap();
    assert!(list.truncated);
    assert_eq!(list.tracks.len(), PLAYLIST_LIMIT);
}

#[test]
fn single_track_listing_carries_its_stream() {
    let url = Url::parse("https://example.com/watch").unwrap();
    // A lone video's listing is its full record, so it comes back ready to
    // play and the resolver never has to run.
    let list = parse_list(
        br#"{"title":"Song","url":"https://cdn.example.com/audio","acodec":"opus"}"#,
        &url,
    )
    .unwrap();
    assert!(matches!(&list.tracks[0].target, Target::Url(actual) if actual == &url));
    // A lone video's title is the track's, not a playlist's.
    assert_eq!(list.playlist_name, None);
    let stream = list.tracks[0]
        .stream
        .as_ref()
        .expect("a single video carries its stream");
    assert_eq!(stream.media_url, "https://cdn.example.com/audio");
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

    assert!(matches!(parse_output(json), Err(YtDlpError::NoOpusFormat)));
}

#[test]
fn errors_on_empty_output() {
    assert!(matches!(
        parse_output(b"\n  \n"),
        Err(YtDlpError::NoMetadata)
    ));
}

#[test]
fn estimated_filesize_is_not_a_download_limit() {
    let resolved = parse_output(
        br#"{"url":"https://example.com/audio","acodec":"opus","filesize_approx":131072}"#,
    )
    .unwrap();
    assert_eq!(resolved.content_length, None);
    let resolved = parse_output(br#"{"url":"https://example.com/audio","acodec":"opus","filesize":524288,"filesize_approx":131072}"#).unwrap();
    assert_eq!(resolved.content_length, Some(524288));
}

#[test]
fn signed_urls_expire_when_their_signature_does() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let at = |secs: u64| Some(UNIX_EPOCH + Duration::from_secs(secs));

    // Both shapes googlevideo hands out.
    assert_eq!(
        signed_url_expiry(&format!(
            "https://cdn.googlevideo.com/videoplayback?expire={}",
            now + 3600
        )),
        at(now + 3600)
    );
    assert_eq!(
        signed_url_expiry(&format!(
            "https://cdn.googlevideo.com/expire/{}/id/x",
            now + 3600
        )),
        at(now + 3600)
    );

    // Nothing to go on.
    assert_eq!(signed_url_expiry("https://cdn/song"), None);
    assert_eq!(signed_url_expiry("not a url"), None);
    assert_eq!(signed_url_expiry("https://cdn/song?expire=soon"), None);
}

#[test]
fn streams_go_stale_a_margin_before_they_expire() {
    let stream = |expires_in: Option<i64>| {
        let mut stream = parse_output(br#"{"url":"https://cdn/song","acodec":"opus"}"#).unwrap();
        stream.expires_at = expires_in.map(|secs| {
            let now = SystemTime::now();
            if secs >= 0 {
                now + Duration::from_secs(secs.unsigned_abs())
            } else {
                now - Duration::from_secs(secs.unsigned_abs())
            }
        });
        stream
    };

    let hour = stream(Some(3600));
    assert!(hour.is_fresh());
    let left = hour.time_left().unwrap();
    assert!(
        left > Duration::from_secs(3400) && left < Duration::from_secs(3500),
        "{left:?}"
    );

    // Already gone, or so close it is not worth handing out.
    for secs in [-10, 30] {
        assert_eq!(stream(Some(secs)).time_left(), Some(Duration::ZERO));
        assert!(!stream(Some(secs)).is_fresh());
    }

    // No expiry known: usable, and the caller picks a ceiling.
    assert_eq!(stream(None).time_left(), None);
    assert!(stream(None).is_fresh());
}
