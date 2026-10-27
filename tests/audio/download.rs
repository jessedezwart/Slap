use super::*;

#[test]
fn windows_are_bounded_and_clamped_to_the_file() {
    // Short file: one window covering all of it.
    assert_eq!(window_range(0, Some(1024)), "bytes=0-1023");
    assert_eq!(window_range(512, Some(1024)), "bytes=512-1023");

    // Long file: a bounded window, never the whole thing.
    assert_eq!(
        window_range(0, Some(WINDOW_BYTES * 4)),
        format!("bytes=0-{}", WINDOW_BYTES - 1)
    );
}

#[test]
fn windows_stay_bounded_without_a_length() {
    assert_eq!(
        window_range(0, None),
        format!("bytes=0-{}", WINDOW_BYTES - 1)
    );
    assert_eq!(
        window_range(64, Some(0)),
        format!("bytes=64-{}", 64 + WINDOW_BYTES - 1)
    );
}

#[test]
fn total_is_read_from_content_range() {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_RANGE, "bytes 0-1023/146800640".parse().unwrap());
    assert_eq!(total_from_content_range(&headers), Some(146_800_640));

    // An unknown total must not be mistaken for a real one.
    headers.insert(CONTENT_RANGE, "bytes 0-1023/*".parse().unwrap());
    assert_eq!(total_from_content_range(&headers), None);

    assert_eq!(total_from_content_range(&HeaderMap::new()), None);
}
