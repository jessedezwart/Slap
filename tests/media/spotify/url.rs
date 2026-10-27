use super::*;

#[test]
fn playlist_album_and_track_links_and_unsupported_spotify_links() {
    for path in [
        "playlist/1234567890123456789012?si=abc",
        "intl-nl/playlist/1234567890123456789012/",
    ] {
        assert_eq!(
            link(&Url::parse(&format!("https://open.spotify.com/{path}")).unwrap()).unwrap(),
            Link::Playlist("1234567890123456789012")
        );
    }
    for path in [
        "track/2I67vuiIAcmcchh7037NDP?si=39ae27ead6f94925",
        "intl-de/track/2I67vuiIAcmcchh7037NDP",
    ] {
        assert_eq!(
            link(&Url::parse(&format!("https://open.spotify.com/{path}")).unwrap()).unwrap(),
            Link::Track("2I67vuiIAcmcchh7037NDP")
        );
    }
    assert_eq!(
        link(&Url::parse("https://open.spotify.com/album/1234567890123456789012?si=x").unwrap())
            .unwrap(),
        Link::Album("1234567890123456789012")
    );
    for raw in [
        "https://spotify.link/abc",
        "https://open.spotify.com/artist/1234567890123456789012",
        "https://open.spotify.com/playlist/invalid",
        "https://open.spotify.com/track/invalid",
        "https://example.com/playlist/1234567890123456789012",
    ] {
        assert!(link(&Url::parse(raw).unwrap()).is_err());
    }
    assert!(!is_spotify_url(
        &Url::parse("https://open.spotify.com.evil.test/playlist/x").unwrap()
    ));
}
