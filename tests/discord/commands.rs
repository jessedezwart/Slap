use super::*;

#[test]
fn queue_pages_are_bounded_and_keep_positions() {
    let tracks: Vec<_> = (0..13)
        .map(|i| {
            Arc::new(TrackMetadata {
                title: Some(format!("Song {i}")),
                ..TrackMetadata::default()
            })
        })
        .collect();
    let visible = std::iter::once(tracks[0].clone())
        .chain(tracks[6..11].iter().cloned())
        .collect::<Vec<_>>();
    let page = format_queue(&visible, tracks.len(), 2);
    assert!(page.contains("Current track:** Song 0"));
    assert!(page.contains("page 2/3"));
    assert!(page.contains("6. Song 6"));
    assert!(page.contains("10. Song 10"));
    assert!(!page.contains("11. Song 11"));
    assert!(format_queue(&tracks, tracks.len(), usize::MAX).contains("does not exist"));
    assert_eq!(format_queue(&[], 0, 1), "The queue is empty.");
    let metadata = Arc::new(TrackMetadata {
        title: Some("*@\n".repeat(100)),
        ..TrackMetadata::default()
    });
    let long = vec![metadata; 10];
    let message = format_queue(&long, long.len(), 1);
    assert!(message.chars().count() < 2000);
    assert_eq!(message.lines().count(), 8);
}

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
