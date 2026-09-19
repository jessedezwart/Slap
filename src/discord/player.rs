//! One lightweight message updater per voice session. No media lookups, image
//! downloads, per-second HTTP polling, or network work on Songbird's driver.

use std::{sync::Arc, time::Duration};

use songbird::tracks::{PlayMode, ReadyState, TrackState};
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use twilight_model::{
    channel::{
        Message,
        message::{
            AllowedMentions, Component,
            component::{ActionRow, Button, ButtonStyle},
        },
    },
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, MessageMarker},
    },
};

use super::commands::{escape, format_duration};
use crate::{audio::ytdlp::TrackMetadata, discord::state::State, voice::session::GuildSession};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(15);
const MIN_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn button_command(custom_id: &str) -> Option<&'static str> {
    match custom_id {
        "player:skip" => Some("skip"),
        "player:stop" => Some("stop"),
        "player:shuffle" => Some("shuffle"),
        "player:queue" => Some("queue"),
        _ => None,
    }
}

pub async fn start(
    state: Arc<State>,
    guild_id: Id<GuildMarker>,
    cancel: &CancellationToken,
    message: Message,
    notice: String,
) {
    let Some(session) = state
        .sessions
        .attach_panel(guild_id, cancel, message.id)
        .await
    else {
        return;
    };
    let task_state = Arc::clone(&state);
    state.player_tasks.spawn(async move {
        // Also release ownership if this task panics, so another /play can retry.
        let _lease = PanelLease {
            state: Arc::clone(&task_state),
            guild_id,
            message_id: message.id,
        };
        run(
            task_state,
            guild_id,
            message.channel_id,
            message.id,
            session,
            notice,
        )
        .await;
    });
}

struct PanelLease {
    state: Arc<State>,
    guild_id: Id<GuildMarker>,
    message_id: Id<MessageMarker>,
}

impl Drop for PanelLease {
    fn drop(&mut self) {
        self.state
            .sessions
            .release_panel(self.guild_id, self.message_id);
    }
}

async fn run(
    state: Arc<State>,
    guild_id: Id<GuildMarker>,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    session: GuildSession,
    notice: String,
) {
    let mut ticks = time::interval_at(Instant::now() + PROGRESS_INTERVAL, PROGRESS_INTERVAL);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut next_update = Instant::now();
    let mut previous = None;
    let mut first = true;
    let mut failures = 0u32;
    loop {
        if !first {
            tokio::select! {
                biased;
                () = session.cancel.cancelled() => break,
                () = session.updates.notified() => {},
                _ = ticks.tick() => {},
            }
        }
        first = false;
        // Notify stores a single permit: bursts do not allocate a queue of work.
        // This limit also applies to event-triggered edits and failed requests.
        tokio::select! {
            biased;
            () = session.cancel.cancelled() => break,
            () = time::sleep_until(next_update) => {},
        }
        next_update = Instant::now() + MIN_UPDATE_INTERVAL;
        let snapshot = tokio::select! {
            biased;
            () = session.cancel.cancelled() => break,
            snapshot = state.sessions.player_snapshot(guild_id, message_id) => snapshot,
        };
        let Some((track, upcoming)) = snapshot else {
            break;
        };
        let view = if let Some(track) = track {
            // get_info is a driver message/oneshot, not an HTTP request. Never
            // wait for it while holding a session, queue, or Call lock.
            let info = tokio::select! {
                biased;
                () = session.cancel.cancelled() => break,
                info = time::timeout(Duration::from_secs(1), track.get_info()) => info,
            };
            let Ok(Ok(info)) = info else {
                continue;
            };
            View::playing(&track.data::<TrackMetadata>(), &info, upcoming, &notice)
        } else {
            View::empty(&notice)
        };
        if previous.as_ref() == Some(&view) {
            continue;
        }
        if session.cancel.is_cancelled() {
            break;
        }
        // Serialize edits, including the final disabled state. No lock is held
        // during HTTP, and requests cannot strand this task indefinitely.
        let update_components = previous
            .as_ref()
            .is_none_or(|old: &View| old.components != view.components);
        let result = edit(&state, channel_id, message_id, &view, update_components).await;
        next_update = Instant::now() + MIN_UPDATE_INTERVAL;
        match result {
            Ok(()) => {
                previous = Some(view);
                failures = 0;
            }
            Err(EditError::Gone) => return,
            Err(EditError::Retry) => {
                // A timed-out edit may still have reached Discord. Resend the
                // entire view next time rather than trusting the old buttons.
                previous = None;
                failures = (failures + 1).min(3);
                next_update = Instant::now() + Duration::from_secs(15 * (1 << (failures - 1)));
            }
        }
    }
    let _ = edit(&state, channel_id, message_id, &View::ended(), true).await;
}

enum EditError {
    Gone,
    Retry,
}

async fn edit(
    state: &State,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    view: &View,
    update_components: bool,
) -> Result<(), EditError> {
    let mentions = AllowedMentions::default();
    let request = state
        .http
        .update_message(channel_id, message_id)
        .content(Some(&view.content))
        .allowed_mentions(Some(&mentions));
    // Progress-only edits send just the small text payload, not the buttons.
    let request = if update_components {
        request.components(Some(&view.components))
    } else {
        request
    };
    match time::timeout(HTTP_TIMEOUT, request).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => {
            // Deleted message / lost access: stop polling. A later /play can
            // claim a new panel. Use the bot token so edits work after 15 min.
            if matches!(error.kind(), twilight_http::error::ErrorType::Response { status, .. }
                if matches!(status.get(), 403 | 404))
            {
                warn!(%message_id, %error, "player message is no longer accessible");
                return Err(EditError::Gone);
            }
            warn!(%message_id, %error, "failed to update player message; backing off");
            Err(EditError::Retry)
        }
        Err(_) => {
            warn!(%message_id, "player message update timed out; backing off");
            Err(EditError::Retry)
        }
    }
}

#[derive(Debug, PartialEq)]
struct View {
    content: String,
    components: Vec<Component>,
}

impl View {
    fn playing(metadata: &TrackMetadata, info: &TrackState, upcoming: usize, notice: &str) -> Self {
        let status = if info.playing.is_done() {
            "Changing tracks…"
        } else if info.playing == PlayMode::Pause {
            "Paused"
        } else if info.ready != ReadyState::Playable {
            "Loading…"
        } else {
            "Now playing"
        };
        let title = escape(
            metadata
                .title
                .as_deref()
                .or(metadata.webpage_url.as_deref())
                .unwrap_or("Unknown track"),
        );
        Self {
            content: format!(
                "**{status}**\n**{title}**\n{}\n{upcoming} upcoming tracks{notice}",
                progress(info.position, metadata.duration)
            ),
            components: buttons(false, false, upcoming),
        }
    }

    fn empty(notice: &str) -> Self {
        Self {
            content: format!(
                "**Queue finished**\nAdd more music with `/play`, or press Stop to disconnect.{notice}"
            ),
            components: buttons(false, true, 0),
        }
    }

    fn ended() -> Self {
        Self {
            content: "**Playback ended**\nUse `/play` to start a new session.".into(),
            components: buttons(true, true, 0),
        }
    }
}

fn progress(position: Duration, duration: Option<Duration>) -> String {
    let Some(duration) = duration.filter(|duration| !duration.is_zero()) else {
        return format!("`{} elapsed • duration unknown`", format_duration(position));
    };
    let position = position.min(duration);
    let filled = ((position.as_secs_f64() / duration.as_secs_f64()) * 16.0) as usize;
    format!(
        "`{}{} {} / {}`",
        "━".repeat(filled),
        "─".repeat(16 - filled),
        format_duration(position),
        format_duration(duration)
    )
}

fn buttons(ended: bool, empty: bool, upcoming: usize) -> Vec<Component> {
    vec![Component::ActionRow(ActionRow {
        id: None,
        components: [
            ("skip", "Skip", ButtonStyle::Primary, empty),
            ("stop", "Stop", ButtonStyle::Danger, false),
            ("shuffle", "Shuffle", ButtonStyle::Secondary, upcoming < 2),
            ("queue", "Show queue", ButtonStyle::Secondary, false),
        ]
        .into_iter()
        .map(|(command, label, style, disabled)| {
            Component::Button(Button {
                id: None,
                custom_id: Some(format!("player:{command}")),
                label: Some(label.into()),
                style,
                disabled: ended || disabled,
                emoji: None,
                url: None,
                sku_id: None,
            })
        })
        .collect(),
    })]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_handles_unknown_zero_and_overrun() {
        assert_eq!(
            progress(Duration::from_secs(30), Some(Duration::from_secs(60))),
            "`━━━━━━━━──────── 0:30 / 1:00`"
        );
        assert_eq!(
            progress(Duration::from_secs(90), Some(Duration::from_secs(60))),
            "`━━━━━━━━━━━━━━━━ 1:00 / 1:00`"
        );
        for duration in [None, Some(Duration::ZERO)] {
            assert_eq!(
                progress(Duration::from_secs(30), duration),
                "`0:30 elapsed • duration unknown`"
            );
        }
    }

    #[test]
    fn paused_views_are_stable_and_titles_are_bounded() {
        let metadata = TrackMetadata {
            title: Some("*@everyone\n".repeat(1000)),
            duration: Some(Duration::from_secs(60)),
            ..Default::default()
        };
        let info = TrackState {
            playing: PlayMode::Pause,
            position: Duration::from_secs(30),
            ready: ReadyState::Playable,
            ..Default::default()
        };
        let view = View::playing(&metadata, &info, 500, "");
        assert!(view.content.starts_with("**Paused**"));
        assert!(!view.content.contains("@everyone"));
        assert!(view.content.chars().count() < 1000);
        assert_eq!(view, View::playing(&metadata, &info, 500, ""));
        assert!(
            View::playing(&metadata, &TrackState::default(), 0, "")
                .content
                .starts_with("**Loading…**")
        );
    }

    #[test]
    fn controls_disable_for_empty_and_ended_sessions() {
        for (view, expected) in [
            (View::empty(""), [true, false, true, false]),
            (View::ended(), [true; 4]),
        ] {
            let Component::ActionRow(row) = &view.components[0] else {
                panic!()
            };
            for (component, disabled) in row.components.iter().zip(expected) {
                let Component::Button(button) = component else {
                    panic!()
                };
                assert_eq!(button.disabled, disabled);
                assert!(button_command(button.custom_id.as_deref().unwrap()).is_some());
            }
        }
        assert_eq!(button_command("player:destroy"), None);
    }
}
