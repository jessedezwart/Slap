//! Per-guild voice sessions and their teardown.
//!
//! Each guild gets one [`CancellationToken`]. Every yt-dlp invocation, every
//! HTTP fetch task and every buffer below it holds a child of that token, so
//! tearing a guild down is a single `cancel()` rather than a walk over
//! per-track state.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use rand::seq::SliceRandom;

use dashmap::DashMap;
use songbird::{
    Songbird,
    error::JoinError,
    events::{CoreEvent, TrackEvent},
    input::Input,
    tracks::{Track, TrackHandle},
};
use tokio::sync::{
    Mutex, OwnedMutexGuard,
    mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker},
};

use crate::audio::ytdlp::TrackMetadata;
use crate::voice::events::{DriverDisconnectNotifier, TrackErrorNotifier, VoiceCommand};

/// How far into a track the next one starts loading, matching the lead
/// songbird's own `enqueue` would have picked.
const PRELOAD_LEAD: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct GuildSession {
    pub channel_id: Id<ChannelMarker>,
    pub cancel: CancellationToken,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("could not join the voice channel: {0}")]
    Join(#[from] JoinError),

    #[error("not connected to a voice channel in this guild")]
    NotConnected,

    #[error("join my voice channel to control playback")]
    WrongChannel,

    #[error("nothing is playing")]
    EmptyQueue,

    #[error("the track finished before the command could be applied")]
    TrackFinished,
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
            call.add_global_event(TrackEvent::Error.into(), TrackErrorNotifier);
        }

        self.sessions.insert(
            guild_id,
            GuildSession {
                channel_id,
                cancel: cancel.clone(),
            },
        );

        if let Err(error) = self.songbird.join(guild_id, channel_id).await {
            self.teardown_locked(guild_id, "join failed").await;
            return Err(error.into());
        }

        info!(%guild_id, %channel_id, "joined voice channel");

        Ok(cancel)
    }

    /// Append a complete import atomically, rejecting work cancelled by stop/leave.
    pub async fn enqueue(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        tracks: Vec<(Input, TrackMetadata)>,
    ) -> Result<usize, SessionError> {
        let _guard = self.lock(guild_id).await;
        if cancel.is_cancelled() || self.channel_of(guild_id).is_none() {
            return Err(SessionError::NotConnected);
        }
        let call = self
            .songbird
            .get(guild_id)
            .ok_or(SessionError::NotConnected)?;
        let mut call = call.lock().await;
        let position = call.queue().len() + 1;
        for (input, metadata) in tracks {
            let preload = metadata
                .duration
                .map(|duration| duration.saturating_sub(PRELOAD_LEAD));
            let track = Track::new_with_data(input, Arc::new(metadata));
            call.enqueue_with_preload(track, preload);
        }
        Ok(position)
    }

    /// Voice controls share the lifecycle lock with joins, enqueues and teardown.
    pub async fn control(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        command: &str,
    ) -> Result<ControlResult, SessionError> {
        let _guard = self.lock(guild_id).await;
        let current = self
            .channel_of(guild_id)
            .ok_or(SessionError::NotConnected)?;
        if current != channel_id {
            return Err(SessionError::WrongChannel);
        }
        if command == "stop" {
            self.teardown_locked(guild_id, "stop command").await;
            return Ok(ControlResult::Message(
                "Stopped playback, cleared the queue, and disconnected.".into(),
            ));
        }
        let call = self
            .songbird
            .get(guild_id)
            .ok_or(SessionError::NotConnected)?;
        let call = call.lock().await;
        let queue = call.queue();
        if command == "queue" {
            return Ok(ControlResult::Queue(queue.current_queue()));
        }
        if queue.is_empty() {
            return Err(SessionError::EmptyQueue);
        }
        let message = match command {
            "pause" => {
                queue.pause().map_err(|_| SessionError::TrackFinished)?;
                "Paused. Use `/resume` to continue.".into()
            }
            "resume" => {
                queue.resume().map_err(|_| SessionError::TrackFinished)?;
                "Resumed playback.".into()
            }
            "skip" => {
                queue.skip().map_err(|_| SessionError::TrackFinished)?;
                "Skipped the current track.".into()
            }
            "shuffle" => {
                let count = queue.modify_queue(shuffle_upcoming);
                if count < 2 {
                    "There are fewer than two upcoming tracks to shuffle.".into()
                } else {
                    format!("Shuffled {count} upcoming tracks.")
                }
            }
            _ => unreachable!("only registered controls reach this method"),
        };
        Ok(ControlResult::Message(message))
    }

    /// Evaluate against current state after acquiring the lifecycle lock.
    pub async fn teardown_if(
        &self,
        guild_id: Id<GuildMarker>,
        reason: &str,
        condition: impl FnOnce() -> bool,
    ) {
        let _guard = self.lock(guild_id).await;
        if condition() {
            self.teardown_locked(guild_id, reason).await;
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

        info!(%guild_id, reason, "tore down voice pipeline");
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

pub enum ControlResult {
    Message(String),
    Queue(Vec<TrackHandle>),
}

fn shuffle_upcoming<T>(queue: &mut VecDeque<T>) -> usize {
    let upcoming = queue.make_contiguous().get_mut(1..).unwrap_or_default();
    upcoming.shuffle(&mut rand::rng());
    upcoming.len()
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
mod tests {
    use super::*;

    fn test_manager() -> Arc<SessionManager> {
        // No gateway or voice connection is opened in these lifecycle tests.
        let songbird = Songbird::twilight_from_config(
            Arc::new(songbird::shards::TwilightMap::new(Default::default())),
            Id::<twilight_model::id::marker::UserMarker>::new(100),
            songbird::Config::default(),
        );
        SessionManager::new(Arc::new(songbird)).0
    }

    #[tokio::test]
    async fn stop_rejects_other_channels_and_cancels_pending_work() {
        let manager = test_manager();
        let guild = Id::new(1);
        let channel = Id::new(10);
        let cancel = CancellationToken::new();
        let child = cancel.child_token();
        manager.sessions.insert(
            guild,
            GuildSession {
                channel_id: channel,
                cancel: cancel.clone(),
            },
        );
        assert!(matches!(
            manager.control(guild, Id::new(20), "stop").await,
            Err(SessionError::WrongChannel)
        ));
        assert!(!child.is_cancelled());
        assert!(manager.control(guild, channel, "stop").await.is_ok());
        assert!(child.is_cancelled());
        assert_eq!(manager.channel_of(guild), None);

        // A late playlist result cannot append to the next voice session.
        let replacement = CancellationToken::new();
        manager.sessions.insert(
            guild,
            GuildSession {
                channel_id: channel,
                cancel: replacement.clone(),
            },
        );
        assert!(matches!(
            manager.enqueue(guild, &cancel, vec![]).await,
            Err(SessionError::NotConnected)
        ));
        manager
            .teardown_if(guild, "stale disconnect", || !cancel.is_cancelled())
            .await;
        assert!(!replacement.is_cancelled());
        assert_eq!(manager.channel_of(guild), Some(channel));
        manager.teardown(guild, "test cleanup").await;
    }

    #[test]
    fn shuffle_preserves_current_and_all_upcoming_tracks() {
        let mut queue: VecDeque<_> = (0..100).collect();
        assert_eq!(shuffle_upcoming(&mut queue), 99);
        assert_eq!(queue.front(), Some(&0));
        let mut items: Vec<_> = queue.into_iter().collect();
        items.sort_unstable();
        assert_eq!(items, (0..100).collect::<Vec<_>>());
        assert_eq!(shuffle_upcoming(&mut VecDeque::<u8>::new()), 0);
        assert_eq!(shuffle_upcoming(&mut VecDeque::from([1])), 0);
        let mut two = VecDeque::from([1, 2]);
        assert_eq!(shuffle_upcoming(&mut two), 1);
        assert_eq!(two, VecDeque::from([1, 2]));
    }
}
