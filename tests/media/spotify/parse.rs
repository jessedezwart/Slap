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
    let Target::Search(query) = &parsed.target else {
        panic!("a Spotify track is searched for");
    };
    assert_eq!(query.key, "4cOdK2wGLETKBW3PvgPWqT");
    assert_eq!(query.artist, "Artist, Guest");
    assert_eq!(query.title, "Song & punctuation?!");
    assert_eq!(query.duration, Some(Duration::from_millis(123456)));
    let metadata = &parsed.metadata;
    assert_eq!(
        metadata.title.as_deref(),
        Some("Artist, Guest — Song & punctuation?!")
    );
    assert_eq!(metadata.uploader.as_deref(), Some("Artist, Guest"));
    assert_eq!(
        metadata.thumbnail.as_deref(),
        Some("https://i.scdn.co/image/large")
    );
    assert_eq!(
        metadata.webpage_url.as_ref().map(url::Url::as_str),
        Some("https://open.spotify.com/track/example")
    );
    assert!(parsed.stream.is_none());
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
