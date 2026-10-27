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

use std::{collections::VecDeque, sync::Arc, time::Duration};

use dashmap::DashMap;
use songbird::{
    Songbird,
    error::JoinError,
    events::{CoreEvent, TrackEvent},
    input::Input,
    tracks::{Track, TrackHandle},
};
use tokio::sync::{
    Mutex, Notify, OwnedMutexGuard,
    mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker},
};

use crate::voice::events::{DriverDisconnectNotifier, PlayerNotifier, VoiceCommand};
use crate::voice::queue::QueueEntry;

/// How far into a track the next one starts loading, matching the lead
/// songbird's own `enqueue` would have picked.
const PRELOAD_LEAD: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct GuildSession {
    pub channel_id: Id<ChannelMarker>,
    pub cancel: CancellationToken,
    pub updates: Arc<Notify>,
}

/// What the player shows, taken under the lifecycle lock.
pub struct PlayerSnapshot {
    pub track: Option<TrackHandle>,
    pub upcoming: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("could not join the voice channel: {0}")]
    Join(#[from] JoinError),

    #[error("not connected to a voice channel in this guild")]
    NotConnected,

    #[error("caller is in a different voice channel")]
    WrongChannel,

    #[error("the queue is empty")]
    EmptyQueue,

    #[error("the track finished before the control was applied")]
    TrackFinished,

    /// The session the caller started with has been torn down.
    #[error("the voice session ended")]
    Ended,
}

pub struct SessionManager {
    songbird: Arc<Songbird>,
    sessions: DashMap<Id<GuildMarker>, GuildSession>,
    commands: UnboundedSender<VoiceCommand>,
    locks: DashMap<Id<GuildMarker>, Arc<Mutex<()>>>,
}

impl SessionManager {
    pub fn new(songbird: Arc<Songbird>) -> (Arc<Self>, UnboundedReceiver<VoiceCommand>) {
        let (commands, receiver) = mpsc::unbounded_channel();

        let manager = Arc::new(Self {
            songbird,
            sessions: DashMap::new(),
            commands,
            locks: DashMap::new(),
        });

        (manager, receiver)
    }

    async fn lock(&self, guild_id: Id<GuildMarker>) -> OwnedMutexGuard<()> {
        let lock = self.locks.entry(guild_id).or_default().clone();
        lock.lock_owned().await
    }

    /// Join `channel_id`, reusing the existing connection when it is already
    /// the right one.
    ///
    /// Returns the guild's cancellation token, which callers hand to every
    /// task they spawn for this guild.
    pub async fn join(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
    ) -> Result<CancellationToken, SessionError> {
        let _guard = self.lock(guild_id).await;
        if let Some(session) = self.sessions.get(&guild_id) {
            let reusable = session.channel_id == channel_id
                && !session.cancel.is_cancelled()
                && self.songbird.get(guild_id).is_some();

            if reusable {
                return Ok(session.cancel.clone());
            }
        }

        // Reject requests from another channel; recover stale sessions in place.
        if self
            .channel_of(guild_id)
            .is_some_and(|current| current != channel_id)
        {
            return Err(SessionError::WrongChannel);
        }
        self.teardown_locked(guild_id, "reconnecting").await;

        let cancel = CancellationToken::new();
        let updates = Arc::new(Notify::new());
        let call = self.songbird.get_or_insert(guild_id);

        {
            let mut call = call.lock().await;

            call.deafen(true).await?;
            call.remove_all_global_events();
            call.add_global_event(
                CoreEvent::DriverDisconnect.into(),
                DriverDisconnectNotifier {
                    guild_id,
                    commands: self.commands.clone(),
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
                    PlayerNotifier {
                        guild_id,
                        event,
                        updates: Arc::clone(&updates),
                    },
                );
            }
        }

        self.sessions.insert(
            guild_id,
            GuildSession {
                channel_id,
                cancel: cancel.clone(),
                updates,
            },
        );

        if let Err(error) = self.songbird.join(guild_id, channel_id).await {
            self.teardown_locked(guild_id, "join failed").await;
            return Err(error.into());
        }

        debug!(%guild_id, %channel_id, "joined voice channel");

        Ok(cancel)
    }

    /// Add a complete import atomically, rejecting work cancelled by stop/leave.
    ///
    /// Returns the first track's queue position, counting the playing track as
    /// position 1.
    pub async fn enqueue(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        tracks: Vec<(Input, QueueEntry)>,
        mode: QueueMode,
    ) -> Result<usize, SessionError> {
        let _guard = self.lock(guild_id).await;
        if cancel.is_cancelled() {
            return Err(SessionError::NotConnected);
        }
        if !self.sessions.contains_key(&guild_id) {
            return Err(SessionError::NotConnected);
        }
        let call = self
            .songbird
            .get(guild_id)
            .ok_or(SessionError::NotConnected)?;
        let mut call = call.lock().await;
        if mode == QueueMode::Replace {
            // Stops the current track and drops the rest, freeing their buffers.
            call.queue().stop();
        }
        let count = tracks.len();
        let appended_at = call.queue().len() + 1;
        for (input, entry) in tracks {
            let preload = entry
                .metadata
                .duration
                .map(|duration| duration.saturating_sub(PRELOAD_LEAD));
            let track = Track::new_with_data(input, Arc::new(entry));
            call.enqueue_with_preload(track, preload);
        }
        let position = if mode == QueueMode::Next && appended_at > 1 {
            call.queue().modify_queue(|queue| play_next(queue, count));
            2
        } else {
            appended_at
        };
        self.notify_player(guild_id);
        Ok(position)
    }

    /// Voice controls share the lifecycle lock with joins, enqueues and teardown.
    ///
    /// `session` is the token the caller's view of the session was built from;
    /// a control from a session that has since ended is refused.
    pub async fn control(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        session: Option<&CancellationToken>,
        control: Control,
    ) -> Result<(), SessionError> {
        let _guard = self.lock(guild_id).await;
        self.check_caller(guild_id, channel_id, session)?;
        if control == Control::Stop {
            self.teardown_locked(guild_id, "stop command").await;
            return Ok(());
        }
        let call = self
            .songbird
            .get(guild_id)
            .ok_or(SessionError::NotConnected)?;
        let call = call.lock().await;
        let queue = call.queue();
        if queue.is_empty() {
            return Err(SessionError::EmptyQueue);
        }
        match control {
            Control::Pause => queue.pause(),
            Control::Resume => queue.resume(),
            Control::Skip => queue.skip(),
            Control::Stop => unreachable!("handled above"),
        }
        .map_err(|_| SessionError::TrackFinished)?;
        // No player notify: songbird's own track events report the change.
        debug!(%guild_id, %channel_id, ?control, remaining = queue.len(), "playback control applied");
        Ok(())
    }

    /// The playing track, then up to `len` upcoming ones.
    pub async fn queue_head(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        session: Option<&CancellationToken>,
        len: usize,
    ) -> Result<QueueHead, SessionError> {
        let _guard = self.lock(guild_id).await;
        self.check_caller(guild_id, channel_id, session)?;
        let call = self
            .songbird
            .get(guild_id)
            .ok_or(SessionError::NotConnected)?;
        let call = call.lock().await;
        Ok(call.queue().modify_queue(|queue| {
            let total = queue.len();
            // Clone only the listed tracks, regardless of playlist size.
            let tracks = queue
                .iter()
                .take(1 + len)
                .map(|track| track.handle())
                .collect();
            QueueHead { tracks, total }
        }))
    }

    /// Refuse a caller whose session has ended or who is in another channel.
    /// Call with the lifecycle lock held, so the answer holds for the action.
    fn check_caller(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        session: Option<&CancellationToken>,
    ) -> Result<(), SessionError> {
        // An old player must never control a replacement session, even after
        // a reconnect to the same channel.
        if session.is_some_and(CancellationToken::is_cancelled) {
            return Err(SessionError::Ended);
        }
        let current = self
            .channel_of(guild_id)
            .ok_or(SessionError::NotConnected)?;
        if current != channel_id {
            return Err(SessionError::WrongChannel);
        }
        Ok(())
    }

    /// Title of the current track, for human-facing control activity logs.
    pub async fn current_track_title(&self, guild_id: Id<GuildMarker>) -> Option<String> {
        let _guard = self.lock(guild_id).await;
        let call = self.songbird.get(guild_id)?;
        let call = call.lock().await;
        call.queue()
            .current()
            .and_then(|track| track.data::<QueueEntry>().metadata.title.clone())
    }

    /// Wake the player for a change songbird has no event for, like the queue
    /// growing.
    fn notify_player(&self, guild_id: Id<GuildMarker>) {
        if let Some(session) = self.sessions.get(&guild_id) {
            session.updates.notify_one();
        }
    }

    /// The session `cancel` belongs to, while it is still the current one.
    pub async fn session(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
    ) -> Option<GuildSession> {
        let _guard = self.lock(guild_id).await;
        if cancel.is_cancelled() {
            return None;
        }
        self.sessions.get(&guild_id).map(|session| session.clone())
    }

    /// Constant-size snapshot of the session `cancel` belongs to; all locks
    /// are released before querying the driver or making a Discord request.
    pub async fn player_snapshot(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
    ) -> Option<PlayerSnapshot> {
        let _guard = self.lock(guild_id).await;
        if cancel.is_cancelled() {
            return None;
        }
        let call = self.songbird.get(guild_id)?;
        let call = call.lock().await;
        Some(call.queue().modify_queue(|queue| PlayerSnapshot {
            track: queue.front().map(|track| track.handle()),
            upcoming: queue.len().saturating_sub(1),
        }))
    }

    /// Evaluate against current state after acquiring the lifecycle lock.
    pub async fn teardown_if(
        &self,
        guild_id: Id<GuildMarker>,
        reason: &str,
        condition: impl FnOnce() -> bool,
    ) -> bool {
        let _guard = self.lock(guild_id).await;
        if condition() {
            self.teardown_locked(guild_id, reason).await;
            true
        } else {
            false
        }
    }

    /// Drop everything belonging to `guild_id`.
    ///
    /// Idempotent: the session is removed from the map first, so the
    /// disconnect our own `leave` provokes does not recurse.
    pub async fn teardown(&self, guild_id: Id<GuildMarker>, reason: &str) {
        let _guard = self.lock(guild_id).await;
        self.teardown_locked(guild_id, reason).await;
    }

    async fn teardown_locked(&self, guild_id: Id<GuildMarker>, reason: &str) {
        let Some((_, session)) = self.sessions.remove(&guild_id) else {
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

    /// Channel we believe we are connected to, if any.
    pub fn channel_of(&self, guild_id: Id<GuildMarker>) -> Option<Id<ChannelMarker>> {
        self.sessions
            .get(&guild_id)
            .map(|session| session.channel_id)
    }

    /// Guilds we currently hold a session for.
    pub fn active_guilds(&self) -> Vec<Id<GuildMarker>> {
        self.sessions.iter().map(|entry| *entry.key()).collect()
    }
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

/// A playback control from the player or a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Skip,
    /// Clear the queue and leave the channel.
    Stop,
}

/// The front of the queue, from [`SessionManager::queue_head`].
pub struct QueueHead {
    /// The playing track first, then the requested upcoming ones.
    pub tracks: Vec<TrackHandle>,
    /// Every queued track, the playing one included.
    pub total: usize,
}

/// Move the `count` tracks just appended to the front of the upcoming ones,
/// leaving the playing track at the head of the queue.
fn play_next<T>(queue: &mut VecDeque<T>, count: usize) {
    let start = queue.len().saturating_sub(count);
    if start <= 1 {
        return;
    }
    // Each removal shifts the remaining import down by one and the insertion
    // shifts it back, so the next track to move stays at `start + offset`.
    for offset in 0..count {
        let Some(track) = queue.remove(start + offset) else {
            break;
        };
        queue.insert(1 + offset, track);
    }
}

/// Drains [`VoiceCommand`]s off songbird's driver task.
pub async fn run_command_loop(
    manager: Arc<SessionManager>,
    mut commands: UnboundedReceiver<VoiceCommand>,
) {
    while let Some(command) = commands.recv().await {
        match command {
            VoiceCommand::Teardown {
                guild_id,
                reason,
                cancel,
            } => {
                manager
                    .teardown_if(guild_id, &reason, || !cancel.is_cancelled())
                    .await;
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/voice/session.rs"]
mod tests;
