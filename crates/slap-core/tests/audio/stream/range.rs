use super::*;

#[test]
fn windows_are_bounded_and_clamped_to_the_file() {
    let window = MIN_WINDOW_BYTES;

    // Short file: one window covering all of it.
    assert_eq!(
        window_range(0, Some(1024), window),
        ("bytes=0-1023".into(), 1024)
    );
    assert_eq!(
        window_range(512, Some(1024), window),
        ("bytes=512-1023".into(), 512)
    );

    // Long file: a bounded window, never the whole thing.
    let (range, requested) = window_range(0, Some(window * 4), window);
    assert_eq!(range, format!("bytes=0-{}", window - 1));
    assert_eq!(requested, window);
}

#[test]
fn windows_stay_bounded_without_a_length() {
    let window = MIN_WINDOW_BYTES;

    assert_eq!(
        window_range(0, None, window),
        (format!("bytes=0-{}", window - 1), window)
    );
    assert_eq!(
        window_range(64, Some(0), window),
        (format!("bytes=64-{}", 64 + window - 1), window)
    );
}

/// A window never falls below the floor, and grows with a deep buffer so a
/// window is still drained in about one buffer's worth of playback.
#[test]
fn window_is_sized_from_the_buffer() {
    assert_eq!(window_bytes(1024), MIN_WINDOW_BYTES);
    assert_eq!(window_bytes(MIN_WINDOW_BYTES as usize), MIN_WINDOW_BYTES);
    assert_eq!(window_bytes(4 * 1024 * 1024), 4 * 1024 * 1024);
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
