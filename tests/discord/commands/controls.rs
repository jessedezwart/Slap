use super::*;

#[test]
fn queue_pages_are_bounded_and_keep_positions() {
    let owned: Vec<_> = (0..13)
        .map(|i| Metadata {
            title: Some(format!("Song {i}")),
            ..Metadata::default()
        })
        .collect();
    let tracks: Vec<&Metadata> = owned.iter().collect();
    let visible = std::iter::once(tracks[0])
        .chain(tracks[6..11].iter().copied())
        .collect::<Vec<_>>();
    let page = format_queue(&visible, tracks.len(), 2);
    assert!(page.contains("Current track:** Song 0"));
    assert!(page.contains("page 2/3"));
    assert!(page.contains("6. Song 6"));
    assert!(page.contains("10. Song 10"));
    assert!(!page.contains("11. Song 11"));
    assert!(format_queue(&tracks, tracks.len(), usize::MAX).contains("does not exist"));
    assert_eq!(format_queue(&[], 0, 1), "The queue is empty.");
    let metadata = Metadata {
        title: Some("*@\n".repeat(100)),
        ..Metadata::default()
    };
    let long = vec![&metadata; 10];
    let message = format_queue(&long, long.len(), 1);
    assert!(message.chars().count() < 2000);
    assert_eq!(message.lines().count(), 8);
}
