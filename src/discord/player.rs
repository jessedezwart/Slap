use std::{fmt::Write as _, sync::Arc, time::Duration};

use songbird::tracks::{PlayMode, ReadyState, TrackState};
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use twilight_model::{
    channel::{
        Message,
        message::{
            AllowedMentions, Component, EmojiReactionType, MessageFlags,
            component::{
                ActionRow, Button, ButtonStyle, Container, Section, Separator,
                SeparatorSpacingSize, TextDisplay, Thumbnail, UnfurledMediaItem,
            },
        },
    },
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, MessageMarker},
    },
};

use super::commands::{escape, format_duration};
use crate::{
    audio::ytdlp::TrackMetadata,
    discord::state::State,
    voice::session::{GuildSession, PlayerSnapshot},
};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(15);
const MIN_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn button_command(custom_id: &str) -> Option<&'static str> {
    match custom_id {
        "player:pause" => Some("pause"),
        "player:resume" => Some("resume"),
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
        let Some(PlayerSnapshot {
            track,
            upcoming,
            shuffle,
        }) = snapshot
        else {
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
            View::playing(
                &track.data::<TrackMetadata>(),
                &info,
                upcoming,
                shuffle,
                &notice,
                session.channel_id,
            )
        } else {
            View::empty(shuffle, &notice)
        };
        if previous.as_ref() == Some(&view) {
            continue;
        }
        if session.cancel.is_cancelled() {
            break;
        }
        // Serialize edits, including the final delete. No lock is held
        // during HTTP, and requests cannot strand this task indefinitely.
        let result = edit(&state, channel_id, message_id, &view).await;
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
    // Stopped, disconnected or superseded: a dead player is just clutter.
    match time::timeout(
        HTTP_TIMEOUT,
        state.http.delete_message(channel_id, message_id),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!(%message_id, %error, "failed to delete player message"),
        Err(_) => warn!(%message_id, "player message delete timed out"),
    }
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
) -> Result<(), EditError> {
    let mentions = AllowedMentions::default();
    // The first edit converts the plain /play reply into a V2 message, which
    // requires dropping its text content. Discord never converts it back.
    let request = state
        .http
        .update_message(channel_id, message_id)
        .content(None)
        .flags(MessageFlags::IS_COMPONENTS_V2)
        .components(Some(&view.components))
        .allowed_mentions(Some(&mentions));
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

const ACCENT_PLAYING: u32 = 0x8B5CF6;
const ACCENT_PAUSED: u32 = 0xF0B232;
const ACCENT_IDLE: u32 = 0x80848E;

/// A Components V2 message: everything lives in one accented container.
#[derive(Debug, PartialEq)]
struct View {
    components: Vec<Component>,
}

impl View {
    fn playing(
        metadata: &TrackMetadata,
        info: &TrackState,
        upcoming: usize,
        shuffle: bool,
        notice: &str,
        voice_channel: Id<ChannelMarker>,
    ) -> Self {
        let paused = info.playing == PlayMode::Pause;
        let (status, accent) = if info.playing.is_done() {
            ("Changing tracks…", ACCENT_IDLE)
        } else if paused {
            ("Paused", ACCENT_PAUSED)
        } else if info.ready != ReadyState::Playable {
            ("Loading…", ACCENT_IDLE)
        } else {
            ("Now playing", ACCENT_PLAYING)
        };
        let title = escape(
            metadata
                .title
                .as_deref()
                .or(metadata.webpage_url.as_ref().map(url::Url::as_str))
                .unwrap_or("Unknown track"),
        );
        let mut details = match metadata
            .webpage_url
            .as_ref()
            .map(url::Url::as_str)
            .filter(|url| url.starts_with("https://") && !url.contains('>'))
        {
            Some(url) => format!("**[{title}](<{url}>)**"),
            None => format!("**{title}**"),
        };
        if let Some(user) = metadata.requester {
            let _ = write!(details, "\n- Added by <@{user}>");
        }
        let _ = write!(details, "\n- <#{voice_channel}>");
        let details = match metadata.thumbnail.as_deref() {
            Some(url) if url.starts_with("https://") => Component::Section(Section {
                id: None,
                components: vec![text(details)],
                accessory: Box::new(Component::Thumbnail(Thumbnail {
                    id: None,
                    media: UnfurledMediaItem {
                        url: url.to_owned(),
                        proxy_url: None,
                        height: None,
                        width: None,
                        content_type: None,
                    },
                    description: None,
                    spoiler: None,
                })),
            }),
            _ => text(details),
        };
        let mut components = vec![
            text(format!("### {status}")),
            separator(true),
            details,
            text(format!(
                "{} · Queue size: `{upcoming}` · Shuffle: `{}`",
                time(info.position, metadata.duration),
                if shuffle { "On" } else { "Off" }
            )),
        ];
        push_notice(&mut components, notice);
        components.push(buttons(false, paused, shuffle));
        Self::container(accent, components)
    }

    fn empty(shuffle: bool, notice: &str) -> Self {
        let mut components = vec![
            text("### Queue finished".into()),
            separator(true),
            text("Add more music with `/play`, or press Stop to disconnect.".into()),
        ];
        push_notice(&mut components, notice);
        components.push(buttons(true, false, shuffle));
        Self::container(ACCENT_IDLE, components)
    }

    fn container(accent: u32, components: Vec<Component>) -> Self {
        Self {
            components: vec![Component::Container(Container {
                id: None,
                accent_color: Some(Some(accent)),
                spoiler: None,
                components,
            })],
        }
    }
}

fn text(content: String) -> Component {
    Component::TextDisplay(TextDisplay { id: None, content })
}

fn separator(divider: bool) -> Component {
    Component::Separator(Separator {
        id: None,
        divider: Some(divider),
        spacing: Some(SeparatorSpacingSize::Small),
    })
}

fn push_notice(components: &mut Vec<Component>, notice: &str) {
    let notice = notice.trim();
    if !notice.is_empty() {
        components.push(text(format!("-# {notice}")));
    }
}

fn time(position: Duration, duration: Option<Duration>) -> String {
    let Some(duration) = duration.filter(|duration| !duration.is_zero()) else {
        return format!("`{}`", format_duration(position));
    };
    format!(
        "`{} / {}`",
        format_duration(position.min(duration)),
        format_duration(duration)
    )
}

fn buttons(empty: bool, paused: bool, shuffle: bool) -> Component {
    let (toggle, toggle_label, toggle_emoji) = if paused {
        ("resume", "Resume", "▶️")
    } else {
        ("pause", "Pause", "⏸️")
    };
    Component::ActionRow(ActionRow {
        id: None,
        components: [
            (toggle, toggle_label, toggle_emoji, empty, false),
            ("skip", "Skip", "⏭️", empty, false),
            ("stop", "Stop", "⏹️", false, false),
            ("shuffle", "Shuffle", "🔀", empty, shuffle),
            ("queue", "Queue", "📜", false, false),
        ]
        .into_iter()
        .map(|(command, label, emoji, disabled, active)| {
            Component::Button(Button {
                id: None,
                custom_id: Some(format!("player:{command}")),
                label: Some(label.into()),
                // Highlighted while its mode is on.
                style: if active {
                    ButtonStyle::Primary
                } else {
                    ButtonStyle::Secondary
                },
                disabled,
                emoji: Some(EmojiReactionType::Unicode { name: emoji.into() }),
                url: None,
                sku_id: None,
            })
        })
        .collect(),
    })
}

#[cfg(test)]
#[path = "../../tests/discord/player.rs"]
mod tests;
