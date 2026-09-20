use super::*;

fn query(secs: Option<u64>) -> SpotifyTrack {
    SpotifyTrack {
        id: "2Foc5Q5nqNiosCNqttzHof".into(),
        artists: vec!["Daft Punk".into()],
        title: "Get Lucky".into(),
        duration: secs.map(Duration::from_secs),
        album_art: None,
        url: None,
    }
}

fn found(secs: Option<u64>) -> TrackMetadata {
    TrackMetadata {
        duration: secs.map(Duration::from_secs),
        ..TrackMetadata::default()
    }
}

#[test]
fn accepts_durations_within_tolerance() {
    // 7% of 369s is about 25.8s.
    for secs in [369, 370, 344, 394] {
        assert!(matches(&query(Some(369)), &found(Some(secs))), "{secs}s");
    }
    // Short tracks get at least 10s.
    assert!(matches(&query(Some(60)), &found(Some(70))));
}

#[test]
fn rejects_other_lengths() {
    // The radio edit when the album version was asked for, and
    // SoundCloud's 30-second preview.
    for secs in [248, 30, 343, 395] {
        assert!(!matches(&query(Some(369)), &found(Some(secs))), "{secs}s");
    }
    assert!(!matches(&query(Some(60)), &found(Some(71))));
}

#[test]
fn unknown_durations_are_accepted() {
    assert!(matches(&query(None), &found(Some(30))));
    assert!(matches(&query(Some(369)), &found(None)));
}
