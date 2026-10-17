//! Per-guild voice sessions and their teardown.
//!
//! Each guild gets one [`CancellationToken`]. Every yt-dlp invocation, every
//! HTTP fetch task and every buffer below it holds a child of that token, so
//! tearing a guild down is a single `cancel()` rather than a walk over
//! per-track state.

use std::{sync::Arc, time::Duration};

use dashmap::DashMap;
use songbird::{
    Songbird,
    error::JoinError,
    events::{CoreEvent, TrackEvent},
    input::Input,
    tracks::TrackHandle,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker},
};

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
}

pub struct SessionManager {
    songbird: Arc<Songbird>,
    sessions: DashMap<Id<GuildMarker>, GuildSession>,
    commands: UnboundedSender<VoiceCommand>,
}

impl SessionManager {
    pub fn new(songbird: Arc<Songbird>) -> (Arc<Self>, UnboundedReceiver<VoiceCommand>) {
        let (commands, receiver) = mpsc::unbounded_channel();

        let manager = Arc::new(Self {
            songbird,
            sessions: DashMap::new(),
            commands,
        });

        (manager, receiver)
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
        if let Some(session) = self.sessions.get(&guild_id) {
            let reusable = session.channel_id == channel_id
                && !session.cancel.is_cancelled()
                && self.songbird.get(guild_id).is_some();

            if reusable {
                return Ok(session.cancel.clone());
            }
        }

        // Moving channels or recovering from a stale entry: start clean, so the
        // old pipeline is never left running against the new connection.
        self.teardown(guild_id, "reconnecting").await;

        let call = self.songbird.join(guild_id, channel_id).await?;
        let cancel = CancellationToken::new();

        {
            let mut call = call.lock().await;

            call.remove_all_global_events();
            call.add_global_event(
                CoreEvent::DriverDisconnect.into(),
                DriverDisconnectNotifier {
                    guild_id,
                    commands: self.commands.clone(),
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

        info!(%guild_id, %channel_id, "joined voice channel");

        Ok(cancel)
    }

    /// Add a track to the guild's queue, returning its position.
    ///
    /// Position 1 means it started playing immediately.
    ///
    /// `duration` is passed in rather than resolved here: songbird's own
    /// `enqueue` awaits the track's metadata while holding the `Call` lock,
    /// and a shard's `Songbird::process` would queue behind that.
    pub async fn enqueue(
        &self,
        guild_id: Id<GuildMarker>,
        input: Input,
        duration: Option<Duration>,
    ) -> Result<(TrackHandle, usize), SessionError> {
        let call = self
            .songbird
            .get(guild_id)
            .ok_or(SessionError::NotConnected)?;

        let preload = duration.map(|duration| duration.saturating_sub(PRELOAD_LEAD));

        let mut call = call.lock().await;
        let handle = call.enqueue_with_preload(input.into(), preload);

        Ok((handle, call.queue().len()))
    }

    /// Drop everything belonging to `guild_id`.
    ///
    /// Idempotent: the session is removed from the map first, so the
    /// disconnect our own `leave` provokes does not recurse.
    pub async fn teardown(&self, guild_id: Id<GuildMarker>, reason: &str) {
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

/// Drains [`VoiceCommand`]s off songbird's driver task.
pub async fn run_command_loop(
    manager: Arc<SessionManager>,
    mut commands: UnboundedReceiver<VoiceCommand>,
) {
    while let Some(command) = commands.recv().await {
        match command {
            VoiceCommand::Teardown { guild_id, reason } => {
                manager.teardown(guild_id, &reason).await;
            }
        }
    }
}
