//! What a session has played.

use std::{sync::Mutex, time::Duration};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayStats {
    /// Tracks that played for any time at all.
    pub tracks: usize,
    /// Time spent actually playing, pauses excluded.
    pub duration: Duration,
}

/// Running tally of a session's [`PlayStats`], frozen once the session ends.
#[derive(Default)]
pub struct PlayTally(Mutex<Tally>);

#[derive(Default)]
struct Tally {
    stats: PlayStats,
    finished: bool,
}

impl PlayTally {
    /// Count a track that played for `play_time`.
    pub fn record(&self, play_time: Duration) {
        self.lock().record(play_time);
    }

    /// Count the track still playing, if any, then stop counting so stopping it is not counted twice.
    pub fn finish(&self, playing: Option<Duration>) {
        let mut tally = self.lock();
        if let Some(play_time) = playing {
            tally.record(play_time);
        }
        tally.finished = true;
    }

    pub fn stats(&self) -> PlayStats {
        self.lock().stats
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
