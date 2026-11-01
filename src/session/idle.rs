//! Leaving the voice channel when nothing is playing.

use std::{sync::Weak, time::Duration};

use songbird::tracks::PlayMode;
use tokio::time;
use tokio_util::sync::CancellationToken;
use twilight_model::id::{Id, marker::GuildMarker};

use super::{SessionManager, still_current};

/// How often a session checks whether it is idle.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// How long the driver gets to report whether the current track is paused.
const INFO_TIMEOUT: Duration = Duration::from_secs(1);

impl SessionManager {
    /// Tear the session down once nothing has played for `idle_timeout`.
    pub(super) async fn leave_when_idle(
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
}
