use super::*;

#[test]
fn playlist_links_and_unsupported_spotify_links() {
    for path in [
        "playlist/1234567890123456789012?si=abc",
        "intl-nl/playlist/1234567890123456789012/",
    ] {
        assert_eq!(
            playlist_id(&Url::parse(&format!("https://open.spotify.com/{path}")).unwrap()).unwrap(),
            "1234567890123456789012"
        );
    }
    for raw in [
        "https://spotify.link/abc",
        "https://open.spotify.com/album/1234567890123456789012",
        "https://open.spotify.com/playlist/invalid",
        "https://example.com/playlist/1234567890123456789012",
    ] {
        assert!(playlist_id(&Url::parse(raw).unwrap()).is_err());
    }
    assert!(!is_spotify_url(
        &Url::parse("https://open.spotify.com.evil.test/playlist/x").unwrap()
    ));
}
