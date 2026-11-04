//! Per-guild voice sessions.

mod events;
mod idle;
mod ownership;
mod queue;
mod stats;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dashmap::DashMap;
use songbird::{Call, Songbird, error::JoinError};
use tokio::sync::{Mutex, Notify, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker},
};

#[cfg(test)]
pub use self::stats::StartedWith;
pub use self::{
    ownership::Control,
    queue::{Origin, Prefetch, QueueEntry, QueueHead, QueueMode},
    stats::{PlayStats, PlayTally, Played},
};
use crate::history::PlayHistory;

/// How long teardown waits for the current track's play time.
const PLAY_TIME_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct GuildSession {
    pub channel_id: Id<ChannelMarker>,
    pub cancel: CancellationToken,
    /// Wakes the player task to redraw.
    pub redraw: Arc<Notify>,
    /// What this session has played, complete once `cancel` fires.
    pub played: Arc<PlayTally>,
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

    #[error("Bot is shutting down, try again in a moment.")]
    Closed,
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
    /// Set at shutdown, after which no session can start.
    closed: AtomicBool,
    /// Where each session's stats are added once it ends, if anywhere.
    history: Option<Arc<PlayHistory>>,
}

impl SessionManager {
    pub fn new(
        songbird: Arc<Songbird>,
        idle_timeout: Duration,
        history: Option<Arc<PlayHistory>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            songbird,
            guilds: DashMap::new(),
            idle_timeout,
            closed: AtomicBool::new(false),
            history,
        })
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
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
        if self.closed.load(Ordering::SeqCst) {
            return Err(SessionError::Closed);
        }
        // Does session already exist?
        if let Some(current) = session.as_ref() {
            // Ran from a different channel?
            if current.channel_id != channel_id {
                return Err(SessionError::WrongChannel);
            }
            // Already in a channel, return the existing session token
            if !current.cancel.is_cancelled() && self.songbird.get(guild_id).is_some() {
                debug!(%guild_id, %channel_id, "reusing session");
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
        let played = Arc::new(PlayTally::default());

        // Create the call
        let call = self.songbird.get_or_insert(guild_id);
        // Modify the call
        {
            // Get the lock, call is shadowed to prevent unintentional edits
            let mut call = call.lock().await;

            // Set to deafen for privacy and bitrate
            call.deafen(true).await?;
            events::attach(
                &mut call,
                guild_id,
                Arc::downgrade(self),
                &cancel,
                &redraw,
                &played,
            );
            // Lock goes out of scope and is dropped
        }

        *session = Some(GuildSession {
            channel_id,
            cancel: cancel.clone(),
            redraw,
            played,
            player_component_id: None,
        });

        if let Err(error) = self.songbird.join(guild_id, channel_id).await {
            self.teardown_locked(guild_id, &mut session, "join failed")
                .await;
            return Err(error.into());
        }

        info!(%guild_id, %channel_id, "session created");

        tokio::spawn(Self::leave_when_idle(
            Arc::downgrade(self),
            guild_id,
            cancel.clone(),
        ));

        Ok(cancel)
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

        let call = self.songbird.get(guild_id);
        // Count the track being cut off before the player task, woken by the cancel, reads the tally.
        let current = match &call {
            Some(call) => call.lock().await.queue().current(),
            None => None,
        };
        let playing = match current {
            Some(track) => match tokio::time::timeout(PLAY_TIME_TIMEOUT, track.get_info()).await {
                Ok(Ok(info)) => Some((info.play_time, track.data::<QueueEntry>())),
                _ => None,
            },
            None => None,
        };
        session.played.finish(
            playing
                .as_ref()
                .map(|(play_time, entry)| (*play_time, &**entry)),
        );

        if let Some(history) = &self.history {
            let history = Arc::clone(history);
            let stats = session.played.stats();
            let write = tokio::task::spawn_blocking(move || history.record(guild_id, &stats));
            if let Err(error) = write.await {
                warn!(%guild_id, %error, "failed to save play history");
            }
        }

        session.cancel.cancel();

        if let Some(call) = call {
            let mut call = call.lock().await;

            call.queue().stop();
            call.remove_all_global_events();
        }

        if let Err(error) = self.songbird.remove(guild_id).await {
            warn!(%guild_id, %error, "failed to remove voice connection");
        }

        info!(%guild_id, reason, "session ended");
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

#[cfg(test)]
#[path = "../tests/session/manager.rs"]
mod tests;
