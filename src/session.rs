//! Per-guild voice sessions.

mod events;

use std::{
    collections::VecDeque,
    sync::{Arc, Weak},
    time::Duration,
};

use dashmap::DashMap;
use songbird::{
    Call, Songbird,
    error::JoinError,
    events::{CoreEvent, TrackEvent},
    input::Input,
    tracks::{PlayMode, Track, TrackHandle, TrackQueue},
};
use tokio::{
    sync::{Mutex, Notify, OwnedMutexGuard},
    time,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
};
use url::Url;

use self::events::{DisconnectHandler, TrackEventHandler};
use crate::media::Metadata;

/// How far into a track the next one starts loading. Due to PO token generation, ytdlp might take a while.
const PRELOAD_LEAD: Duration = Duration::from_secs(10);
/// How often a session checks whether it is idle.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// How long the driver gets to report whether the current track is paused.
const INFO_TIMEOUT: Duration = Duration::from_secs(1);

/// A track
#[derive(Debug)]
pub struct QueueEntry {
    pub metadata: Metadata,
    pub origin: Arc<Origin>,
}

/// Details about the track import request
#[derive(Debug)]
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

#[derive(Clone)]
pub struct GuildSession {
    pub channel_id: Id<ChannelMarker>,
    pub cancel: CancellationToken,
    /// Wakes the player task to redraw.
    pub redraw: Arc<Notify>,
    /// The message showing this session's player, once one has claimed it.
    pub player_component_id: Option<Id<MessageMarker>>,
}

/// User facing messages
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

/// Optional since session might not exist when torn down.
/// Is a mutex to prevent raceconditions (e.g. /play and a /disconnect will trigger after each other)
/// Is arc since getting just the mutex will hold a DashMaps "shard". A shard can hold multiple guilds.
type Slot = Arc<Mutex<Option<GuildSession>>>;

pub struct SessionManager {
    songbird: Arc<Songbird>,
    guilds: DashMap<Id<GuildMarker>, Slot>,
    /// Leave the voice channel after nothing has played for this long.
    idle_timeout: Duration,
}

impl SessionManager {
    pub fn new(songbird: Arc<Songbird>, idle_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            songbird,
            guilds: DashMap::new(),
            idle_timeout,
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

    /// Join `channel_id`, reusing the existing connection when it is already the right one.
    /// Returns the guild's cancellation token, which callers hand to every task they spawn for this guild.
    pub async fn join(
        self: &Arc<Self>,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
    ) -> Result<CancellationToken, SessionError> {
        let mut session = self.lock(guild_id).await;
        // Does session already exist?
        if let Some(current) = session.as_ref() {
            // Ran from a different channel?
            if current.channel_id != channel_id {
                return Err(SessionError::WrongChannel);
            }
            // Already in a channel, return the existing session token
            if !current.cancel.is_cancelled() && self.songbird.get(guild_id).is_some() {
                return Ok(current.cancel.clone());
            }
        }

        // Create a fresh session by tearing down any potential leftovers
        self.teardown_locked(guild_id, &mut session, "reconnecting")
            .await;

        // Create session token
        let cancel = CancellationToken::new();
        // Create notification for a render of the player component
        let redraw = Arc::new(Notify::new());

        // Create the call
        let call = self.songbird.get_or_insert(guild_id);
        // Modify the call
        {
            // Get the lock, call is shadowed to prevent unintentional edits
            let mut call = call.lock().await;

            // Set to deafen for privacy and bitrate
            call.deafen(true).await?;
            // Events are set explicitly
            call.remove_all_global_events();
            // Handle disconnect
            call.add_global_event(
                CoreEvent::DriverDisconnect.into(),
                DisconnectHandler {
                    guild_id,
                    sessions: Arc::downgrade(self),
                    cancel: cancel.clone(),
                },
            );
            // Handle track events
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
            // Lock goes out of scope and is dropped
        }

        *session = Some(GuildSession {
            channel_id,
            cancel: cancel.clone(),
            redraw,
            player_component_id: None,
        });

        if let Err(error) = self.songbird.join(guild_id, channel_id).await {
            self.teardown_locked(guild_id, &mut session, "join failed")
                .await;
            return Err(error.into());
        }

        debug!(%guild_id, %channel_id, "joined voice channel");

        tokio::spawn(Self::leave_when_idle(
            Arc::downgrade(self),
            guild_id,
            cancel.clone(),
        ));

        Ok(cancel)
    }

    /// Tear the session down once nothing has played for `idle_timeout`.
    async fn leave_when_idle(
        sessions: Weak<Self>,
        guild_id: Id<GuildMarker>,
        cancel: CancellationToken,
    ) {
        let mut idle_for = Duration::ZERO;
        loop {
            let interval = match sessions.upgrade() {
                Some(sessions) => sessions.idle_timeout.min(IDLE_CHECK_INTERVAL),
                None => return,
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                () = time::sleep(interval) => {}
            }
            let Some(sessions) = sessions.upgrade() else {
                return;
            };
            // Checked under the session lock, so a concurrent /play cannot be torn down.
            let mut session = sessions.lock(guild_id).await;
            if still_current(&session, &cancel).is_none() {
                return;
            }
            if !sessions.is_idle(guild_id).await {
                idle_for = Duration::ZERO;
                continue;
            }
            idle_for += interval;
            if idle_for >= sessions.idle_timeout {
                sessions
                    .teardown_locked(guild_id, &mut session, "idle timeout")
                    .await;
                return;
            }
        }
    }

    /// Nothing queued, or the current track is paused.
    async fn is_idle(&self, guild_id: Id<GuildMarker>) -> bool {
        let Some(call) = self.songbird.get(guild_id) else {
            return true;
        };
        let Some(track) = call.lock().await.queue().current() else {
            return true;
        };
        // A track still loading counts as playing.
        time::timeout(INFO_TIMEOUT, track.get_info())
            .await
            .is_ok_and(|info| info.is_ok_and(|info| info.playing == PlayMode::Pause))
    }

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

        // Trigger player component redraw
        session.redraw.notify_one();
        Ok(())
    }

    /// Handles interactions
    pub async fn control(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        player_component: Id<MessageMarker>,
        control: Control,
    ) -> Result<(), SessionError> {
        let mut session = self.lock(guild_id).await;
        check_player(&session, channel_id, player_component)?;
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
        debug!(%guild_id, %channel_id, ?control, remaining = queue.len(), "playback control applied");
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

    /// Link player component to session
    pub async fn claim_player_component(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        player_component_id: Id<MessageMarker>,
    ) -> Option<GuildSession> {
        let mut session = self.lock(guild_id).await;
        let session = session
            .as_mut()
            .filter(|session| !cancel.is_cancelled() && session.player_component_id.is_none())?;
        session.player_component_id = Some(player_component_id);
        Some(session.clone())
    }

    pub async fn release_player_component(
        &self,
        guild_id: Id<GuildMarker>,
        message_id: Id<MessageMarker>,
    ) {
        let mut session = self.lock(guild_id).await;
        if let Some(session) = session
            .as_mut()
            .filter(|session| session.player_component_id == Some(message_id))
        {
            session.player_component_id = None;
        }
    }

    /// Evaluates a condition, if passes: teardown
    /// This is done in a function so a lock can be acquired first to prevent race conditions.
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
        // Remove the session from the slot to prevent retriggering teardowns after events.
        let Some(session) = session.take() else {
            return;
        };

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

    pub fn is_known_guild(&self, guild_id: Id<GuildMarker>) -> bool {
        self.guilds.contains_key(&guild_id)
    }

    pub fn known_guilds(&self) -> Vec<Id<GuildMarker>> {
        self.guilds.iter().map(|entry| *entry.key()).collect()
    }
}

/// Returns the session if it matches the cancellation token (so whether it's still current)
fn still_current<'a>(
    session: &'a Option<GuildSession>,
    cancel: &CancellationToken,
) -> Option<&'a GuildSession> {
    session.as_ref().filter(|_| !cancel.is_cancelled())
}

/// Check whether the player component is on the current session and linked to the current channel.
fn check_player(
    session: &Option<GuildSession>,
    channel_id: Id<ChannelMarker>,
    player_component: Id<MessageMarker>,
) -> Result<(), SessionError> {
    let session = session
        .as_ref()
        .filter(|session| session.player_component_id == Some(player_component))
        .ok_or(SessionError::Ended)?;
    if session.channel_id != channel_id {
        return Err(SessionError::WrongChannel);
    }
    Ok(())
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Skip,
    /// Clear the queue and leave the channel.
    Stop,
}

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
