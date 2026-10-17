use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A resolve whose media URL carries `expire`, `secs` from now.
fn resolved(media_url: &str) -> Resolved {
    Resolved {
        media_url: media_url.to_owned(),
        headers: reqwest::header::HeaderMap::new(),
        content_length: None,
        bitrate_bps: None,
        metadata: crate::audio::ytdlp::TrackMetadata {
            title: Some("Song".to_owned()),
            ..Default::default()
        },
    }
}

fn expiring_in(secs: i64) -> String {
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    format!(
        "https://cdn.googlevideo.com/videoplayback?expire={}",
        now + secs
    )
}

fn cache(entries: u64) -> YtdlpResolveCache {
    YtdlpResolveCache::new(ResolveCacheConfig {
        entries,
        max_ttl: Duration::from_secs(600),
    })
}

#[tokio::test]
async fn a_resolve_made_while_listing_is_not_made_again() {
    let cache = cache(8);
    let url = expiring_in(3600);
    cache.insert("https://youtu.be/song", resolved(&url)).await;

    let hit = cache
        .get_or_resolve("https://youtu.be/song", async {
            panic!("the listing already resolved this track")
        })
        .await
        .unwrap();

    assert_eq!(hit.media_url, url);
}

#[test]
fn signed_urls_expire_when_their_signature_does() {
    // Both shapes googlevideo hands out, each less the margin.
    let hour = Duration::from_secs(3600);
    let about = |ttl: Option<Duration>| {
        let ttl = ttl.expect("the URL carries an expiry");
        assert!(
            ttl.abs_diff(hour.saturating_sub(EXPIRY_MARGIN)) < Duration::from_secs(2),
            "{ttl:?}"
        );
    };

    about(get_signed_url_ttl(&expiring_in(3600)));

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    about(get_signed_url_ttl(&format!(
        "https://cdn.googlevideo.com/expire/{}/id/x",
        now + 3600
    )));

    // Already gone, or so close it is not worth handing out.
    assert_eq!(get_signed_url_ttl(&expiring_in(-10)), Some(Duration::ZERO));
    assert_eq!(get_signed_url_ttl(&expiring_in(30)), Some(Duration::ZERO));

    // Nothing to go on: the caller's ceiling applies instead.
    assert_eq!(get_signed_url_ttl("https://cdn/song"), None);
    assert_eq!(get_signed_url_ttl("not a url"), None);
    assert_eq!(get_signed_url_ttl("https://cdn/song?expire=soon"), None);
}

#[tokio::test]
async fn a_live_resolve_is_reused_and_an_expired_one_is_not() {
    let cache = cache(8);
    let counter = AtomicUsize::new(0);
    let runs = &counter;
    let resolve = |url: String| async move {
        runs.fetch_add(1, Ordering::Relaxed);
        Ok(resolved(&url))
    };

    let live = expiring_in(3600);
    for _ in 0..3 {
        let hit = cache
            .get_or_resolve("https://youtu.be/song", resolve(live.clone()))
            .await
            .unwrap();
        assert_eq!(hit.media_url, live);
    }
    assert_eq!(runs.load(Ordering::Relaxed), 1);

    // A different input is a different entry.
    cache
        .get_or_resolve("https://youtu.be/other", resolve(live.clone()))
        .await
        .unwrap();
    assert_eq!(runs.load(Ordering::Relaxed), 2);
    assert_eq!(cache.len().await, 2);

    // A URL that is already spent must not be handed to a second caller.
    let spent = expiring_in(-1);
    for _ in 0..2 {
        cache
            .get_or_resolve("https://youtu.be/spent", resolve(spent.clone()))
            .await
            .unwrap();
    }
    assert_eq!(runs.load(Ordering::Relaxed), 4);
}

#[tokio::test]
async fn concurrent_callers_share_one_run() {
    let cache = cache(8);
    let runs = AtomicUsize::new(0);
    let resolve = || async {
        runs.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(resolved(&expiring_in(3600)))
    };

    let (a, b, c) = tokio::join!(
        cache.get_or_resolve("https://youtu.be/song", resolve()),
        cache.get_or_resolve("https://youtu.be/song", resolve()),
        cache.get_or_resolve("https://youtu.be/song", resolve()),
    );
    let a = a.unwrap();
    assert_eq!(a.media_url, b.unwrap().media_url);
    assert_eq!(a.media_url, c.unwrap().media_url);

    // One yt-dlp for the three of them.
    assert_eq!(runs.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn failures_are_not_cached_and_keep_their_cause() {
    let cache = cache(8);
    let runs = AtomicUsize::new(0);

    let fail = || async {
        runs.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(20)).await;
        Err(AudioError::Timeout(Duration::from_secs(15)))
    };

    // Waiters on a shared run all learn why it failed, and `is_fatal` still
    // sees the cause through the sharing wrapper.
    let (a, b) = tokio::join!(
        cache.get_or_resolve("https://youtu.be/song", fail()),
        cache.get_or_resolve("https://youtu.be/song", fail()),
    );
    for error in [a.unwrap_err(), b.unwrap_err()] {
        assert!(error.is_fatal(), "{error}");
        assert_eq!(error.to_string(), "yt-dlp did not respond within 15s");
    }
    assert_eq!(runs.load(Ordering::Relaxed), 1);
    assert_eq!(cache.len().await, 0);

    // Nothing was stored, so the next caller gets to try again.
    let url = expiring_in(3600);
    let hit = cache
        .get_or_resolve("https://youtu.be/song", async { Ok(resolved(&url)) })
        .await
        .unwrap();
    assert_eq!(hit.media_url, url);
}
