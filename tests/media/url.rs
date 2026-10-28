use super::*;

#[test]
fn accepts_http_urls_from_any_host() {
    for url in [
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "https://youtu.be/dQw4w9WgXcQ",
        "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
        "https://example.com/watch?v=x",
        "http://example.com/audio.opus",
    ] {
        assert!(http_url(url).is_some(), "{url} should be accepted");
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
        assert!(http_url(url).is_none(), "{url} should be rejected");
    }
}

#[test]
fn links_are_imported_and_anything_else_is_searched() {
    assert!(matches!(
        Input::parse(" https://youtu.be/dQw4w9WgXcQ "),
        Ok(Input::Link(url)) if url.as_str() == "https://youtu.be/dQw4w9WgXcQ"
    ));
    // `Artist: Title` parses as a URL with an `artist` scheme.
    for text in [
        "daft punk one more time",
        "Artist: Title",
        "file:///etc/passwd",
    ] {
        assert_eq!(
            Input::parse(&format!("  {text} ")).unwrap(),
            Input::Search(text.to_owned())
        );
    }
    assert!(matches!(Input::parse("   "), Err(MediaError::EmptyInput)));
}
