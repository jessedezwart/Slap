//! Per-guild voice sessions and their teardown.
//!
//! Each guild gets one [`CancellationToken`]. Every yt-dlp invocation, every
//! HTTP fetch task and every buffer below it holds a child of that token, so
//! tearing a guild down is a single `cancel()` rather than a walk over
//! per-track state.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

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
    Mutex, Notify, OwnedMutexGuard,
    mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker},
};

use crate::audio::ytdlp::TrackMetadata;
use crate::voice::events::{DriverDisconnectNotifier, PlayerNotifier, VoiceCommand};

/// How far into a track the next one starts loading, matching the lead
/// songbird's own `enqueue` would have picked.
const PRELOAD_LEAD: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct GuildSession {
    pub channel_id: Id<ChannelMarker>,
    pub cancel: CancellationToken,
    pub updates: Arc<Notify>,
    pub panel_message: Option<Id<MessageMarker>>,
    /// Keep upcoming tracks, including later imports, in random order.
    pub shuffle: bool,
}

/// What the player message shows, taken under the lifecycle lock.
pub struct PlayerSnapshot {
    pub track: Option<TrackHandle>,
    pub upcoming: usize,
    pub shuffle: bool,
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

    #[error("this player has expired; use the current player or a slash command")]
    StalePanel,
}

pub struct SessionManager {
    songbird: Arc<Songbird>,
    sessions: DashMap<Id<GuildMarker>, GuildSession>,
    commands: UnboundedSender<VoiceCommand>,
    locks: DashMap<Id<GuildMarker>, Arc<Mutex<()>>>,
    /// Enqueue order, so turning shuffle off can restore it.
    sequence: AtomicU64,
}

impl SessionManager {
    pub fn new(songbird: Arc<Songbird>) -> (Arc<Self>, UnboundedReceiver<VoiceCommand>) {
        let (commands, receiver) = mpsc::unbounded_channel();

        let manager = Arc::new(Self {
            songbird,
            sessions: DashMap::new(),
            commands,
            locks: DashMap::new(),
            sequence: AtomicU64::new(0),
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
        warning_channel_id: Id<ChannelMarker>,
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
                        commands: self.commands.clone(),
                        cancel: cancel.clone(),
                        warning_channel_id,
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
                panel_message: None,
                shuffle: false,
            },
        );

        if let Err(error) = self.songbird.join(guild_id, channel_id).await {
            self.teardown_locked(guild_id, "join failed").await;
            return Err(error.into());
        }

        debug!(%guild_id, %channel_id, "joined voice channel");

        Ok(cancel)
    }

    /// Append a complete import atomically, rejecting work cancelled by stop/leave.
    ///
    /// Returns the first track's queue position and whether shuffle mode
    /// scattered the import through the queue.
    pub async fn enqueue(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        mut tracks: Vec<(Input, TrackMetadata)>,
    ) -> Result<(usize, bool), SessionError> {
        let _guard = self.lock(guild_id).await;
        if cancel.is_cancelled() {
            return Err(SessionError::NotConnected);
        }
        let shuffle = self
            .sessions
            .get(&guild_id)
            .ok_or(SessionError::NotConnected)?
            .shuffle;
        let call = self
            .songbird
            .get(guild_id)
            .ok_or(SessionError::NotConnected)?;
        let mut call = call.lock().await;
        let position = call.queue().len() + 1;
        for (_, metadata) in &mut tracks {
            metadata.sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        }
        if shuffle {
            // Also randomizes the first track when this import starts playback.
            tracks.shuffle(&mut rand::rng());
        }
        for (input, metadata) in tracks {
            let preload = metadata
                .duration
                .map(|duration| duration.saturating_sub(PRELOAD_LEAD));
            let track = Track::new_with_data(input, Arc::new(metadata));
            call.enqueue_with_preload(track, preload);
        }
        if shuffle {
            call.queue().modify_queue(shuffle_upcoming);
        }
        self.notify_player(guild_id);
        Ok((position, shuffle))
    }

    /// Voice controls share the lifecycle lock with joins, enqueues and teardown.
    pub async fn control(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        command: &str,
        page: usize,
        panel_message: Option<Id<MessageMarker>>,
    ) -> Result<ControlResult, SessionError> {
        let _guard = self.lock(guild_id).await;
        // Check the message identity inside the same lock as the action. An old
        // button must never stop a replacement session, even after a reconnect.
        if let Some(message_id) = panel_message
            && !self.sessions.get(&guild_id).is_some_and(|session| {
                session.panel_message == Some(message_id) && !session.cancel.is_cancelled()
            })
        {
            return Err(SessionError::StalePanel);
        }
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
            return Ok(queue.modify_queue(|queue| {
                let total = queue.len();
                let pages = total.saturating_sub(1).div_ceil(5).max(1);
                if page == 0 || page > pages {
                    return ControlResult::Message(format!(
                        "That page does not exist. Choose a page from 1 to {pages}."
                    ));
                }
                // Clone only the visible page, regardless of playlist size.
                let tracks = queue
                    .front()
                    .into_iter()
                    .chain(queue.range((1 + (page - 1) * 5).min(total)..).take(5))
                    .map(|track| track.handle())
                    .collect();
                ControlResult::Queue {
                    tracks,
                    total,
                    page,
                }
            }));
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
                let mut session = self
                    .sessions
                    .get_mut(&guild_id)
                    .ok_or(SessionError::NotConnected)?;
                session.shuffle = !session.shuffle;
                if session.shuffle {
                    queue.modify_queue(shuffle_upcoming);
                    "Shuffle is on.".into()
                } else {
                    queue.modify_queue(|queue| {
                        restore_order(queue, |track| {
                            track.handle().data::<TrackMetadata>().sequence
                        });
                    });
                    "Shuffle is off.".into()
                }
            }
            _ => unreachable!("only registered controls reach this method"),
        };
        debug!(%guild_id, %channel_id, command, remaining = queue.len(), result = %message, "playback control applied");
        self.notify_player(guild_id);
        Ok(ControlResult::Message(message))
    }

    /// Title of the current track, for human-facing control activity logs.
    pub async fn current_track_title(&self, guild_id: Id<GuildMarker>) -> Option<String> {
        let _guard = self.lock(guild_id).await;
        let call = self.songbird.get(guild_id)?;
        let call = call.lock().await;
        call.queue()
            .current()
            .and_then(|track| track.data::<TrackMetadata>().title.clone())
    }

    fn notify_player(&self, guild_id: Id<GuildMarker>) {
        if let Some(session) = self.sessions.get(&guild_id) {
            session.updates.notify_one();
        }
    }

    /// First successful /play response owns the panel for this session.
    pub async fn attach_panel(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        message_id: Id<MessageMarker>,
    ) -> Option<GuildSession> {
        let _guard = self.lock(guild_id).await;
        if cancel.is_cancelled() {
            return None;
        }
        let mut session = self.sessions.get_mut(&guild_id)?;
        if session.panel_message.is_some() || session.cancel.is_cancelled() {
            return None;
        }
        session.panel_message = Some(message_id);
        Some(session.clone())
    }

    pub fn release_panel(&self, guild_id: Id<GuildMarker>, message_id: Id<MessageMarker>) {
        if let Some(mut session) = self.sessions.get_mut(&guild_id)
            && session.panel_message == Some(message_id)
        {
            session.panel_message = None;
        }
    }

    /// Constant-size snapshot; all locks are released before querying the driver
    /// or making a Discord request.
    pub async fn player_snapshot(
        &self,
        guild_id: Id<GuildMarker>,
        message_id: Id<MessageMarker>,
    ) -> Option<PlayerSnapshot> {
        let _guard = self.lock(guild_id).await;
        let shuffle = self
            .sessions
            .get(&guild_id)
            .filter(|session| {
                session.panel_message == Some(message_id) && !session.cancel.is_cancelled()
            })?
            .shuffle;
        let call = self.songbird.get(guild_id)?;
        let call = call.lock().await;
        Some(call.queue().modify_queue(|queue| PlayerSnapshot {
            track: queue.front().map(|track| track.handle()),
            upcoming: queue.len().saturating_sub(1),
            shuffle,
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

pub enum ControlResult {
    Message(String),
    Queue {
        tracks: Vec<TrackHandle>,
        total: usize,
        page: usize,
    },
}

fn shuffle_upcoming<T>(queue: &mut VecDeque<T>) -> usize {
    let upcoming = queue.make_contiguous().get_mut(1..).unwrap_or_default();
    upcoming.shuffle(&mut rand::rng());
    upcoming.len()
}

/// Undo shuffling, leaving the playing track at the front.
fn restore_order<T>(queue: &mut VecDeque<T>, sequence: impl Fn(&T) -> u64) {
    if let Some(upcoming) = queue.make_contiguous().get_mut(1..) {
        upcoming.sort_by_cached_key(sequence);
    }
}

/// Drains [`VoiceCommand`]s off songbird's driver task.
pub async fn run_command_loop(
    manager: Arc<SessionManager>,
    mut commands: UnboundedReceiver<VoiceCommand>,
    http: Arc<twilight_http::Client>,
) {
    while let Some(command) = commands.recv().await {
        match command {
            VoiceCommand::CookiesRequired {
                guild_id,
                channel_id,
                cancel,
            } => {
                let stopped = manager
                    .teardown_if(guild_id, "YouTube cookies required", || {
                        !cancel.is_cancelled()
                    })
                    .await;
                if stopped {
                    let message = format!(
                        "Playback stopped. {}",
                        crate::error::AudioError::CookiesRequired
                    );
                    let mentions = twilight_model::channel::message::AllowedMentions::default();
                    let request = http
                        .create_message(channel_id)
                        .content(&message)
                        .allowed_mentions(Some(&mentions));
                    match tokio::time::timeout(Duration::from_secs(10), request).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            warn!(%channel_id, %error, "failed to send cookie configuration warning")
                        }
                        Err(_) => warn!(%channel_id, "cookie configuration warning timed out"),
                    }
                }
            }
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
