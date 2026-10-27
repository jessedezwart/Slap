//! What the queue stores per track, on top of the media itself.

use std::sync::Arc;

use twilight_model::id::{Id, marker::UserMarker};
use url::Url;

use crate::media::Metadata;

/// A track in a guild's queue. Stored as songbird track data, so every
/// `TrackHandle::data::<QueueEntry>()` reads it back.
#[derive(Debug)]
pub struct QueueEntry {
    pub metadata: Metadata,
    /// Who queued the track; shown on the player.
    pub requester: Id<UserMarker>,
    /// The playlist this track was imported from, shared by every track of
    /// that import so the player can name it without a second lookup.
    pub playlist: Option<Arc<Playlist>>,
}

/// A named playlist, and the link it was imported from.
#[derive(Debug)]
pub struct Playlist {
    pub name: String,
    pub url: Url,
}
