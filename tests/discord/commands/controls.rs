use super::*;

#[test]
fn queue_lists_ten_upcoming() {
    let owned: Vec<_> = (0..13)
        .map(|i| Metadata {
            title: Some(format!("Song {i}")),
            ..Metadata::default()
        })
        .collect();
    let tracks: Vec<&Metadata> = owned.iter().collect();
    let message = format_queue(&tracks, tracks.len());
    assert!(message.contains("Current track:** Song 0"));
    assert!(message.contains("12 upcoming"));
    assert!(message.contains("1. Song 1"));
    assert!(message.contains("10. Song 10"));
    assert!(!message.contains("Song 11"));
    assert_eq!(format_queue(&[], 0), "The queue is empty.");
    let metadata = Metadata {
        title: Some("*@\n".repeat(100)),
        ..Metadata::default()
    };
    let long = vec![&metadata; 11];
    let message = format_queue(&long, long.len());
    assert!(message.chars().count() < 2000);
    assert_eq!(message.lines().count(), 12);
}
