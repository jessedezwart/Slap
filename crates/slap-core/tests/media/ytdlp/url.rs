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
        assert!(validate_url(url).is_ok(), "{url} should be accepted");
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
        assert!(validate_url(url).is_err(), "{url} should be rejected");
    }
}
