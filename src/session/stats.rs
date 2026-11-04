//! What a session has played.

use std::{sync::Mutex, time::Duration};

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
    /// Count a track that ended after playing for `play_time`.
    pub fn record(&self, play_time: Duration) {
        let mut tally = self.lock();
        if !tally.finished {
            tally.ended += 1;
        }
        tally.record(play_time);
    }

    /// How many tracks have left the queue, so the current track is number `ended() + 1`.
    pub fn ended(&self) -> usize {
        self.lock().ended
    }

    /// Count the track still playing, if any, then stop counting so stopping it is not counted twice.
    pub fn finish(&self, playing: Option<Duration>) {
        let mut tally = self.lock();
        if let Some(play_time) = playing {
            tally.record(play_time);
        }
        tally.finished = true;
    }

    /// Remember `first`, the first track of an import, if nothing was queued before it.
    pub fn start(&self, first: &QueueEntry) {
        let mut tally = self.lock();
        if tally.stats.started_with.is_none() {
            let origin = &first.origin;
            tally.stats.started_with = Some(StartedWith {
                label: origin
                    .playlist_name
                    .clone()
                    .unwrap_or_else(|| first.metadata.display_title().to_owned()),
                url: origin.url.clone(),
                query: origin.query.clone(),
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
    fn record(&mut self, play_time: Duration) {
        if !self.finished && !play_time.is_zero() {
            self.stats.tracks += 1;
            self.stats.duration += play_time;
        }
    }
}
