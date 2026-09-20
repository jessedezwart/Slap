use super::*;

fn path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rust-music-bot-cache-{}-{name}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("cache.redb")
}

#[test]
fn remembers_resolutions_across_reopens() {
    let path = path("roundtrip");
    let cache = SpotifyTrackCache::open(&path).unwrap();
    assert_eq!(cache.get("2Foc5Q5nqNiosCNqttzHof"), None);

    cache.put("2Foc5Q5nqNiosCNqttzHof", "https://youtu.be/5NV6Rdv1a3I");
    assert_eq!(
        cache.get("2Foc5Q5nqNiosCNqttzHof").as_deref(),
        Some("https://youtu.be/5NV6Rdv1a3I")
    );

    // A later run must see what an earlier one resolved.
    drop(cache);
    let reopened = SpotifyTrackCache::open(&path).unwrap();
    assert_eq!(
        reopened.get("2Foc5Q5nqNiosCNqttzHof").as_deref(),
        Some("https://youtu.be/5NV6Rdv1a3I")
    );

    // A re-resolve replaces the entry rather than adding a second one.
    reopened.put("2Foc5Q5nqNiosCNqttzHof", "https://youtu.be/replacement");
    assert_eq!(
        reopened.get("2Foc5Q5nqNiosCNqttzHof").as_deref(),
        Some("https://youtu.be/replacement")
    );

    drop(reopened);
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn reading_an_unknown_track_is_not_an_error() {
    let path = path("missing");
    let cache = SpotifyTrackCache::open(&path).unwrap();
    assert_eq!(cache.get(""), None);
    assert_eq!(cache.get("not-a-real-id"), None);
    drop(cache);
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}
