//! What a session has played.

use std::{sync::Mutex, time::Duration};

use twilight_model::id::{Id, marker::UserMarker};
use url::Url;

use super::QueueEntry;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlayStats {
    /// Tracks that played for any time at all.
    pub tracks: usize,
    /// Time spent actually playing, pauses excluded.
    pub duration: Duration,
    /// The first import queued, if any.
    pub started_with: Option<StartedWith>,
    /// Every track counted in `tracks`, in the order they ended.
    pub played: Vec<Played>,
    /// Every playlist imported, in the order they were queued.
    pub playlists: Vec<Played>,
}

/// The import that started a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedWith {
    /// The playlist's name, or the track's title when it was not a playlist.
    pub label: String,
    /// The link imported, or `None` for a search.
    pub url: Option<Url>,
    /// What was passed to /play.
    pub query: String,
    /// Who queued it.
    pub requester: Id<UserMarker>,
}

/// A track that played, or a playlist that was imported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Played {
    pub label: String,
    pub url: Option<Url>,
}

/// Running tally of a session's [`PlayStats`], frozen once the session ends.
#[derive(Default)]
pub struct PlayTally(Mutex<Tally>);

#[derive(Default)]
struct Tally {
    stats: PlayStats,
    /// Tracks that left the queue, failed ones included.
    ended: usize,
    finished: bool,
}

impl PlayTally {
    /// Count `entry`, a track that ended after playing for `play_time`.
    pub fn record(&self, play_time: Duration, entry: &QueueEntry) {
        let mut tally = self.lock();
        if !tally.finished {
            tally.ended += 1;
        }
        tally.record(play_time, entry);
    }

    /// How many tracks have left the queue, so the current track is number `ended() + 1`.
    pub fn ended(&self) -> usize {
        self.lock().ended
    }

    /// Count the track still playing, if any, then stop counting so stopping it is not counted twice.
    pub fn finish(&self, playing: Option<(Duration, &QueueEntry)>) {
        let mut tally = self.lock();
        if let Some((play_time, entry)) = playing {
            tally.record(play_time, entry);
        }
        tally.finished = true;
    }

    /// Remember the import `first` is the first track of: as the session's start if nothing
    /// was queued before it, and as a playlist if it was one.
    pub fn start(&self, first: &QueueEntry) {
        let mut tally = self.lock();
        let origin = &first.origin;
        if tally.stats.started_with.is_none() {
            tally.stats.started_with = Some(StartedWith {
                label: origin
                    .playlist_name
                    .clone()
                    .unwrap_or_else(|| first.metadata.display_title().to_owned()),
                url: origin.url.clone(),
                query: origin.query.clone(),
                requester: origin.requester,
            });
        }
        if let Some(name) = &origin.playlist_name {
            tally.stats.playlists.push(Played {
                label: name.clone(),
                url: origin.url.clone(),
            });
        }
    }

    pub fn stats(&self) -> PlayStats {
        self.lock().stats.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tally> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl Tally {
    fn record(&mut self, play_time: Duration, entry: &QueueEntry) {
        if !self.finished && !play_time.is_zero() {
            self.stats.tracks += 1;
            self.stats.duration += play_time;
            self.stats.played.push(Played {
                label: entry.metadata.display_title().to_owned(),
                url: entry.metadata.webpage_url.clone(),
            });
        }
    }
}
