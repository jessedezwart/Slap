use super::*;
use serde_json::json;

fn song(name: &str) -> Value {
    json!({"item": {"type":"track", "id":"4cOdK2wGLETKBW3PvgPWqT", "name":name,
        "artists":[{"name":"Artist"},{"name":"Guest"}], "duration_ms":123456,
        "album":{"images":[{"url":"https://i.scdn.co/image/large"}]},
        "external_urls":{"spotify":"https://open.spotify.com/track/example"}}})
}

#[test]
fn reads_the_fields_a_search_needs() {
    let parsed = track(&song("Song & punctuation?!")).unwrap();
    assert_eq!(parsed.id, "4cOdK2wGLETKBW3PvgPWqT");
    assert_eq!(parsed.artists, ["Artist", "Guest"]);
    assert_eq!(parsed.artist(), "Artist, Guest");
    assert_eq!(parsed.title, "Song & punctuation?!");
    assert_eq!(parsed.duration, Some(Duration::from_millis(123456)));
    assert_eq!(
        parsed.album_art.as_deref(),
        Some("https://i.scdn.co/image/large")
    );
    assert_eq!(
        parsed.url.as_deref(),
        Some("https://open.spotify.com/track/example")
    );
}

#[test]
fn accepts_the_legacy_track_field() {
    let mut old = song("Legacy");
    old["track"] = old.as_object_mut().unwrap().remove("item").unwrap();
    assert!(track(&old).is_some());
}

#[test]
fn rejects_entries_that_cannot_be_played() {
    for entry in [
        Value::Null,
        json!({"item":null}),
        json!({"item":{"type":"episode"}}),
        json!({"item":{"type":"track", "name":"", "artists":[]}}),
        // No ID means nothing stable to cache the resolution against.
        json!({"item":{"type":"track", "name":"No ID", "artists":[{"name":"A"}]}}),
    ] {
        assert!(track(&entry).is_none(), "{entry}");
    }
    let mut local = song("Local");
    local["is_local"] = json!(true);
    assert!(track(&local).is_none());
}
