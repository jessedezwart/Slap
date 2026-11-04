//! The track queue of a voice session.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use songbird::{
    Call,
    input::Input,
    tracks::{Track, TrackHandle, TrackQueue},
};
use tokio_util::sync::CancellationToken;
use twilight_model::id::{
    Id,
    marker::{GuildMarker, UserMarker},
};
use url::Url;

use super::{SessionError, SessionManager, still_current};
use crate::media::Metadata;

/// How far into a track the next one starts loading. Due to PO token generation, ytdlp might take a while.
const PRELOAD_LEAD: Duration = Duration::from_secs(10);

/// A track
pub struct QueueEntry {
    pub metadata: Metadata,
    pub origin: Arc<Origin>,
    pub prefetch: Prefetch,
}

/// Starts looking up a queued track's media URL ahead of its turn.
#[derive(Clone, Default)]
pub struct Prefetch(Option<Arc<dyn Fn() + Send + Sync>>);

impl Prefetch {
    pub fn new(start: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(start)))
    }

    fn start(&self) {
        if let Some(start) = &self.0 {
            start();
        }
    }
}

/// Look up the track after the playing one, so it is ready when its turn comes.
pub(super) fn prefetch_next(queue: &TrackQueue) {
    let next = queue.modify_queue(|queue| queue.get(1).map(|track| track.handle()));
    if let Some(next) = next {
        next.data::<QueueEntry>().prefetch.start();
    }
}

/// Details about the track import request
pub struct Origin {
    pub requester: Id<UserMarker>,
    pub requester_name: String,
    /// The link imported, or `None` for a search.
    pub url: Option<Url>,
    pub playlist_name: Option<String>,
    /// Entries skipped for a request
    pub skipped: usize,
    /// More entries existed than were imported.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMode {
    /// Append after everything already queued.
    #[default]
    Append,
    /// Insert directly after the current track.
    Next,
    /// Clear the queue and the current track, then play this import.
    Replace,
}

pub struct QueueHead {
    /// The playing track first, then the requested upcoming ones.
    pub tracks: Vec<TrackHandle>,
    /// Every queued track, the playing one included.
    pub total: usize,
}

impl SessionManager {
    pub async fn enqueue(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        tracks: Vec<(Input, QueueEntry)>,
        mode: QueueMode,
    ) -> Result<(), SessionError> {
        // Get session and call locks
        let session = self.lock(guild_id).await;
        let session = still_current(&session, cancel).ok_or(SessionError::Ended)?;
        let call = self.call(guild_id)?;
        let mut call = call.lock().await;

        // Remove existing queue in case of replace
        if mode == QueueMode::Replace {
            call.queue().stop();
        }

        // Queue the tracks
        let count = tracks.len();
        let was_playing = !call.queue().is_empty();

        for (input, entry) in tracks {
            let preload = entry
                .metadata
                .duration
                .map(|duration| duration.saturating_sub(PRELOAD_LEAD));
            let track = Track::new_with_data(input, Arc::new(entry));
            call.enqueue_with_preload(track, preload);
        }
        if mode == QueueMode::Next && was_playing {
            call.queue().modify_queue(|queue| play_next(queue, count));
        }
        prefetch_next(call.queue());

        // Trigger player component redraw
        session.redraw.notify_one();
        Ok(())
    }

    /// The playing track
    pub async fn now_playing(
        &self,
        guild_id: Id<GuildMarker>,
        len: usize,
        cancel: &CancellationToken,
    ) -> Option<QueueHead> {
        let session = self.lock(guild_id).await;
        still_current(&session, cancel)?;
        let call = self.songbird.get(guild_id)?;
        let call = call.lock().await;
        Some(head_of(&call, len))
    }
}

fn head_of(call: &Call, len: usize) -> QueueHead {
    call.queue().modify_queue(|queue| QueueHead {
        tracks: queue
            .iter()
            .take(1 + len)
            .map(|track| track.handle())
            .collect(),
        total: queue.len(),
    })
}

/// Move the `count` tracks just appended to the front of the upcoming ones,
/// leaving the playing track at the head of the queue.
fn play_next<T>(queue: &mut VecDeque<T>, count: usize) {
    if let Some(upcoming) = queue.make_contiguous().get_mut(1..) {
        upcoming.rotate_right(count.min(upcoming.len()));
    }
}

#[cfg(test)]
#[path = "../../tests/session/queue.rs"]
mod tests;
