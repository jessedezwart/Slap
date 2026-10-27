//! Per-guild voice sessions and their teardown.
//!
//! Each guild gets one [`CancellationToken`]. Every yt-dlp invocation, every
//! HTTP fetch task and every buffer below it holds a child of that token, so
//! tearing a guild down is a single `cancel()` rather than a walk over
//! per-track state.
//!
//! The token doubles as the session's identity: it is cancelled exactly when
//! the session leaves the map, so a caller holding one can tell whether the
//! session it started with is still the current one.

mod events;

use std::{collections::VecDeque, sync::Arc, time::Duration};

use dashmap::DashMap;
use songbird::{
    Call, Songbird,
    error::JoinError,
    events::{CoreEvent, TrackEvent},
    input::Input,
    tracks::{Track, TrackHandle, TrackQueue},
};
use tokio::sync::{Mutex, Notify, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
};
use url::Url;

use self::events::{DisconnectHandler, TrackEventHandler};
use crate::media::Metadata;

/// How far into a track the next one starts loading, matching the lead
/// songbird's own `enqueue` would have picked.
const PRELOAD_LEAD: Duration = Duration::from_secs(5);

/// A track in a guild's queue. Stored as songbird track data, so every
/// `TrackHandle::data::<QueueEntry>()` reads it back.
#[derive(Debug)]
pub struct QueueEntry {
    pub metadata: Metadata,
    /// The import this track came from
    pub origin: Arc<Origin>,
}

/// Details about the track import request
#[derive(Debug)]
pub struct Origin {
    /// Who queued the track; shown on the player.
    pub requester: Id<UserMarker>,
    /// The requester's name as it was when they queued the track.
    pub requester_name: String,
    pub url: Url,
    pub playlist_name: Option<String>,
    pub skipped: usize,
    /// More entries existed than were imported.
    pub truncated: bool,
}

#[derive(Clone)]
pub struct GuildSession {
    pub channel_id: Id<ChannelMarker>,
    pub cancel: CancellationToken,
    /// Wakes the player task to redraw.
    pub redraw: Arc<Notify>,
    /// The message showing this session's player, once one has claimed it.
    pub player_message: Option<Id<MessageMarker>>,
}

/// Messages are written for the person who pressed the button.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("I could not join your voice channel: {0}")]
    Join(#[from] JoinError),

    #[error("I am not connected to a voice channel in this server.")]
    NotConnected,

    #[error("Join my voice channel to control playback.")]
    WrongChannel,

    #[error("Nothing is playing.")]
    EmptyQueue,

    #[error("The track finished before the command could be applied.")]
    TrackFinished,

    /// The session the caller started with, or the player they pressed, has
    /// been torn down.
    #[error("That voice session has ended; use the current player, or /play to start again.")]
    Ended,
}

/// A guild's session behind the lock that orders its joins, enqueues,
/// controls and teardown: holding the guard is holding the session.
type Slot = Arc<Mutex<Option<GuildSession>>>;

pub struct SessionManager {
    songbird: Arc<Songbird>,
    guilds: DashMap<Id<GuildMarker>, Slot>,
}

impl SessionManager {
    pub fn new(songbird: Arc<Songbird>) -> Arc<Self> {
        Arc::new(Self {
            songbird,
            guilds: DashMap::new(),
        })
    }

    async fn lock(&self, guild_id: Id<GuildMarker>) -> OwnedMutexGuard<Option<GuildSession>> {
        let slot = self.guilds.entry(guild_id).or_default().clone();
        slot.lock_owned().await
    }

    fn call(&self, guild_id: Id<GuildMarker>) -> Result<Arc<Mutex<Call>>, SessionError> {
        self.songbird
            .get(guild_id)
            .ok_or(SessionError::NotConnected)
    }

    /// Join `channel_id`, reusing the existing connection when it is already
    /// the right one.
    ///
    /// Returns the guild's cancellation token, which callers hand to every
    /// task they spawn for this guild.
    pub async fn join(
        self: &Arc<Self>,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
    ) -> Result<CancellationToken, SessionError> {
        let mut session = self.lock(guild_id).await;
        // Reject requests from another channel; recover stale sessions in place.
        if let Some(current) = session.as_ref() {
            if current.channel_id != channel_id {
                return Err(SessionError::WrongChannel);
            }
            if !current.cancel.is_cancelled() && self.songbird.get(guild_id).is_some() {
                return Ok(current.cancel.clone());
            }
        }

        self.teardown_locked(guild_id, &mut session, "reconnecting")
            .await;

        let cancel = CancellationToken::new();
        let redraw = Arc::new(Notify::new());
        let call = self.songbird.get_or_insert(guild_id);

        {
            let mut call = call.lock().await;

            call.deafen(true).await?;
            call.remove_all_global_events();
            call.add_global_event(
                CoreEvent::DriverDisconnect.into(),
                DisconnectHandler {
                    guild_id,
                    sessions: Arc::downgrade(self),
                    cancel: cancel.clone(),
                },
            );
            for event in [
                TrackEvent::Play,
                TrackEvent::Pause,
                TrackEvent::End,
                TrackEvent::Error,
                TrackEvent::Playable,
            ] {
                call.add_global_event(
                    event.into(),
                    TrackEventHandler {
                        guild_id,
                        redraw: Arc::clone(&redraw),
                    },
                );
            }
        }

        *session = Some(GuildSession {
            channel_id,
            cancel: cancel.clone(),
            redraw,
            player_message: None,
        });

        if let Err(error) = self.songbird.join(guild_id, channel_id).await {
            self.teardown_locked(guild_id, &mut session, "join failed")
                .await;
            return Err(error.into());
        }

        debug!(%guild_id, %channel_id, "joined voice channel");

        Ok(cancel)
    }

    /// Add a complete import atomically, rejecting work cancelled by stop/leave.
    pub async fn enqueue(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        tracks: Vec<(Input, QueueEntry)>,
        mode: QueueMode,
    ) -> Result<(), SessionError> {
        let session = self.lock(guild_id).await;
        let session = current(&session, cancel).ok_or(SessionError::Ended)?;
        let call = self.call(guild_id)?;
        let mut call = call.lock().await;
        if mode == QueueMode::Replace {
            // Stops the current track and drops the rest, freeing their buffers.
            call.queue().stop();
        }
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
        // Wake the player for a change songbird has no event for, like the
        // queue growing.
        session.redraw.notify_one();
        Ok(())
    }

    /// Voice controls share the lifecycle lock with joins, enqueues and teardown.
    ///
    /// `player_message` is the player the control was pressed on; a control
    /// from a player that is no longer current is refused.
    pub async fn control(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        player_message: Id<MessageMarker>,
        control: Control,
    ) -> Result<(), SessionError> {
        let mut session = self.lock(guild_id).await;
        check_player(&session, channel_id, player_message)?;
        let apply: fn(&TrackQueue) -> _ = match control {
            Control::Stop => {
                self.teardown_locked(guild_id, &mut session, "stop command")
                    .await;
                return Ok(());
            }
            Control::Pause => TrackQueue::pause,
            Control::Resume => TrackQueue::resume,
            Control::Skip => TrackQueue::skip,
        };
        let call = self.call(guild_id)?;
        let call = call.lock().await;
        let queue = call.queue();
        if queue.is_empty() {
            return Err(SessionError::EmptyQueue);
        }
        apply(queue).map_err(|_| SessionError::TrackFinished)?;
        // No player notify: songbird's own track events report the change.
        debug!(%guild_id, %channel_id, ?control, remaining = queue.len(), "playback control applied");
        Ok(())
    }

    /// The playing track, then up to `len` upcoming ones, of the session
    /// `cancel` belongs to; all locks are released before querying the
    /// driver or making a Discord request.
    pub async fn now_playing(
        &self,
        guild_id: Id<GuildMarker>,
        len: usize,
        cancel: &CancellationToken,
    ) -> Option<QueueHead> {
        let session = self.lock(guild_id).await;
        current(&session, cancel)?;
        let call = self.songbird.get(guild_id)?;
        let call = call.lock().await;
        Some(head_of(&call, len))
    }

    /// Make `message_id` the player of the session `cancel` belongs to,
    /// unless that session already has one.
    pub async fn claim_player(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        message_id: Id<MessageMarker>,
    ) -> Option<GuildSession> {
        let mut session = self.lock(guild_id).await;
        let session = session
            .as_mut()
            .filter(|session| !cancel.is_cancelled() && session.player_message.is_none())?;
        session.player_message = Some(message_id);
        Some(session.clone())
    }

    /// Let a later /play claim the player, unless another message has
    /// taken it over since.
    pub async fn release_player(&self, guild_id: Id<GuildMarker>, message_id: Id<MessageMarker>) {
        let mut session = self.lock(guild_id).await;
        if let Some(session) = session
            .as_mut()
            .filter(|session| session.player_message == Some(message_id))
        {
            session.player_message = None;
        }
    }

    /// Evaluate against current state after acquiring the lifecycle lock.
    pub async fn teardown_if(
        &self,
        guild_id: Id<GuildMarker>,
        reason: &str,
        condition: impl FnOnce(&GuildSession) -> bool,
    ) {
        let mut session = self.lock(guild_id).await;
        if session.as_ref().is_some_and(condition) {
            self.teardown_locked(guild_id, &mut session, reason).await;
        }
    }

    /// Drop everything belonging to `guild_id`.
    ///
    /// Idempotent: the session is removed from the map first, so the
    /// disconnect our own `leave` provokes does not recurse.
    pub async fn teardown(&self, guild_id: Id<GuildMarker>, reason: &str) {
        let mut session = self.lock(guild_id).await;
        self.teardown_locked(guild_id, &mut session, reason).await;
    }

    async fn teardown_locked(
        &self,
        guild_id: Id<GuildMarker>,
        session: &mut Option<GuildSession>,
        reason: &str,
    ) {
        let Some(session) = session.take() else {
            return;
        };

        // One cancel aborts every fetch task, closes every connection and frees
        // every buffer for this guild.
        session.cancel.cancel();

        if let Some(call) = self.songbird.get(guild_id) {
            let mut call = call.lock().await;

            call.queue().stop();
            call.remove_all_global_events();
        }

        if let Err(error) = self.songbird.remove(guild_id).await {
            warn!(%guild_id, %error, "failed to remove voice connection");
        }

        debug!(%guild_id, reason, "tore down voice pipeline");
    }

    /// Cheap pre-check: `false` only for a guild that never had a session.
    pub fn may_have_session(&self, guild_id: Id<GuildMarker>) -> bool {
        self.guilds.contains_key(&guild_id)
    }

    /// Every guild that has had a session.
    pub fn known_guilds(&self) -> Vec<Id<GuildMarker>> {
        self.guilds.iter().map(|entry| *entry.key()).collect()
    }
}

/// The session `cancel` belongs to, while it is still the current one.
fn current<'a>(
    session: &'a Option<GuildSession>,
    cancel: &CancellationToken,
) -> Option<&'a GuildSession> {
    session.as_ref().filter(|_| !cancel.is_cancelled())
}

/// Refuse a caller whose player has ended or who is in another channel.
/// Call with the lifecycle lock held, so the answer holds for the action.
fn check_player(
    session: &Option<GuildSession>,
    channel_id: Id<ChannelMarker>,
    player_message: Id<MessageMarker>,
) -> Result<(), SessionError> {
    // An old player must never control a replacement session, even after
    // a reconnect to the same channel.
    let session = session
        .as_ref()
        .filter(|session| session.player_message == Some(player_message))
        .ok_or(SessionError::Ended)?;
    if session.channel_id != channel_id {
        return Err(SessionError::WrongChannel);
    }
    Ok(())
}

fn head_of(call: &Call, len: usize) -> QueueHead {
    call.queue().modify_queue(|queue| QueueHead {
        // Clone only the listed tracks, regardless of playlist size.
        tracks: queue
            .iter()
            .take(1 + len)
            .map(|track| track.handle())
            .collect(),
        total: queue.len(),
    })
}

/// Where a fresh import lands relative to what is already queued.
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

/// A playback control from the player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Skip,
    /// Clear the queue and leave the channel.
    Stop,
}

/// The front of the queue, from [`SessionManager::now_playing`].
pub struct QueueHead {
    /// The playing track first, then the requested upcoming ones.
    pub tracks: Vec<TrackHandle>,
    /// Every queued track, the playing one included.
    pub total: usize,
}

/// Move the `count` tracks just appended to the front of the upcoming ones,
/// leaving the playing track at the head of the queue.
fn play_next<T>(queue: &mut VecDeque<T>, count: usize) {
    if let Some(upcoming) = queue.make_contiguous().get_mut(1..) {
        upcoming.rotate_right(count.min(upcoming.len()));
    }
}

#[cfg(test)]
#[path = "../tests/session/manager.rs"]
mod tests;
