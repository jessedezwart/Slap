use super::*;
use crate::session::{PlayStats, StartedWith};

fn path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "slap-history-{}-{name}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("stats.redb")
}

fn played(label: &str, url: Option<&str>) -> Played {
    Played {
        label: label.into(),
        url: url.map(|url| url.parse().unwrap()),
    }
}

fn session(requester: u64, tracks: &[Played], playlists: &[Played]) -> PlayStats {
    PlayStats {
        tracks: tracks.len(),
        duration: Duration::from_secs(60 * tracks.len() as u64),
        started_with: Some(StartedWith {
            label: "Start".into(),
            url: None,
            query: "start".into(),
            requester: Id::new(requester),
        }),
        played: tracks.to_vec(),
        playlists: playlists.to_vec(),
    }
}

#[test]
fn sums_sessions_per_guild_across_reopens() {
    let path = path("roundtrip");
    let guild = Id::new(1);
    let song = played("Song", Some("https://youtu.be/a"));
    let other = played("Other", None);
    let mix = played("Mix", Some("https://open.spotify.com/playlist/x"));

    let history = PlayHistory::open(&path).unwrap();
    assert_eq!(history.summary(guild, 5), Some(GuildSummary::default()));
    history.record(
        guild,
        &session(7, &[song.clone(), other], std::slice::from_ref(&mix)),
    );
    drop(history);

    let history = PlayHistory::open(&path).unwrap();
    // The title changed since; the link still identifies the track.
    let renamed = played("Song (Remastered)", Some("https://youtu.be/a"));
    history.record(guild, &session(8, &[renamed], &[mix]));
    history.record(guild, &session(8, &[], &[]));
    // Another guild is counted apart, and an empty session not at all.
    history.record(Id::new(2), &session(9, &[song], &[]));
    history.record(guild, &PlayStats::default());

    let summary = history.summary(guild, 5).unwrap();
    assert_eq!(summary.tracks, 3);
    assert_eq!(summary.duration, Duration::from_secs(180));
    assert_eq!(summary.sessions, 3);
    assert_eq!(
        summary.top_tracks,
        vec![
            Ranked {
                label: "Song (Remastered)".into(),
                url: Some("https://youtu.be/a".parse().unwrap()),
                count: 2,
            },
            Ranked {
                label: "Other".into(),
                url: None,
                count: 1,
            },
        ]
    );
    assert_eq!(summary.top_playlists.len(), 1);
    assert_eq!(summary.top_playlists[0].count, 2);
    assert_eq!(summary.top_starters, vec![(Id::new(8), 2), (Id::new(7), 1)]);
    assert_eq!(history.summary(guild, 1).unwrap().top_tracks.len(), 1);

    drop(history);
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}
