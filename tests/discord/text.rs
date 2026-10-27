use super::*;

#[test]
fn formats_durations() {
    assert_eq!(format_duration(Duration::from_secs(5)), "0:05");
    assert_eq!(format_duration(Duration::from_secs(212)), "3:32");
    assert_eq!(format_duration(Duration::from_secs(3_725)), "1:02:05");
}

#[test]
fn escapes_markdown_and_mentions() {
    assert_eq!(escape("a*b_c`d"), "a\\*b\\_c\\`d");
    assert!(escape("@everyone").starts_with("@\u{200b}"));
}

#[test]
fn truncates_long_titles() {
    let long = "x".repeat(500);

    assert_eq!(escape(&long).chars().count(), 200);
}

#[test]
fn playlist_names_are_labels_and_only_https_links_are_embedded() {
    let url = "http://example.com/list".parse().unwrap();
    // The name is a label, not markup.
    assert_eq!(playlist_link("*Road* Trip", &url), "**\\*Road\\* Trip**");
}
