use std::{fmt::Write as _, sync::Arc, sync::atomic::Ordering, time::Duration};

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
    audio::ytdlp::{Playlist, TrackMetadata},
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
        "player:queue" => Some("queue"),
        // A link does not fit on a button, so this one opens the play form.
        "player:add" => Some("play"),
        "player:again" => Some("play-again"),
        _ => None,
    }
}

/// What the Play again button on a stopped player restarts.
pub struct Replay {
    pub url: String,
    pub shuffle: bool,
    /// The imported playlist, when it was one.
    pub playlist: Option<Arc<Playlist>>,
}

/// A stopped player still offering its link, at most one per guild.
///
/// A stop replaces the previous offer, the first press consumes it, and a new
/// player revokes it, so this never grows with the number of players seen.
pub struct Offer {
    pub channel_id: Id<ChannelMarker>,
    pub message_id: Id<MessageMarker>,
    pub replay: Replay,
}

pub async fn start(
    state: Arc<State>,
    guild_id: Id<GuildMarker>,
    cancel: &CancellationToken,
    message: Message,
    notice: String,
    replay: Replay,
) {
    let Some(session) = state
        .sessions
        .attach_panel(guild_id, cancel, message.id)
        .await
    else {
        // A stop can race with Discord returning the initial /play reply.
        if cancel.is_cancelled() {
            delete_player(&state, message.channel_id, message.id).await;
        }
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
        // This session supersedes any stopped player still offering a replay.
        revoke(&task_state, guild_id).await;
        run(
            task_state,
            guild_id,
            message.channel_id,
            message.id,
            session,
            notice,
            replay,
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
    replay: Replay,
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
        let Some(PlayerSnapshot { track, upcoming }) = snapshot else {
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
                &notice,
                session.channel_id,
            )
        } else {
            View::empty(&notice)
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
    let disconnected = session.disconnected.load(Ordering::Acquire);
    stopped(
        &state,
        guild_id,
        channel_id,
        message_id,
        replay,
        disconnected,
    )
    .await;
}

/// Leave the player behind as a stopped one, offering the same link again.
///
/// A disconnect or a shutdown ends the session without anything left to press
/// Play again for, so that button is shown disabled and no offer is registered.
async fn stopped(
    state: &State,
    guild_id: Id<GuildMarker>,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    replay: Replay,
    disconnected: bool,
) {
    let view = View::stopped(&replay, disconnected);
    if disconnected {
        // One attempt: nothing updates this message again, and a player that
        // cannot be rewritten is better deleted than left claiming to play.
        if edit(state, channel_id, message_id, &view).await.is_err() {
            delete_player(state, channel_id, message_id).await;
        }
        return;
    }
    // Register before the edit so a press cannot beat us to the map.
    state.replays.insert(
        guild_id,
        Offer {
            channel_id,
            message_id,
            replay,
        },
    );
    if edit(state, channel_id, message_id, &view).await.is_err() {
        state
            .replays
            .remove_if(&guild_id, |_, offer| offer.message_id == message_id);
        delete_player(state, channel_id, message_id).await;
    }
}

/// Withdraw a guild's replay offer and grey out the button that made it.
///
/// A superseded or shut-down player is left in place, because its `/play` line
/// is still worth copying; only the button it can no longer honour goes dead.
pub async fn revoke(state: &State, guild_id: Id<GuildMarker>) {
    let Some((_, offer)) = state.replays.remove(&guild_id) else {
        return;
    };
    let view = View::stopped(&offer.replay, true);
    if edit(state, offer.channel_id, offer.message_id, &view)
        .await
        .is_err()
    {
        delete_player(state, offer.channel_id, offer.message_id).await;
    }
}

pub(super) async fn delete_player(
    state: &State,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
) {
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
        let mut details = link(&title, metadata.webpage_url.as_ref());
        if let Some(playlist) = metadata.playlist.as_deref() {
            let _ = write!(details, "\n- From {}", playlist_link(playlist));
        }
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
                "{} · Queue size: `{upcoming}`",
                time(info.position, metadata.duration)
            )),
        ];
        push_notice(&mut components, notice);
        components.push(buttons(false, paused));
        Self::container(accent, components)
    }

    fn empty(notice: &str) -> Self {
        let mut components = vec![
            text("### Queue finished".into()),
            separator(true),
            text("Press Add to queue more music, or Stop to disconnect.".into()),
        ];
        push_notice(&mut components, notice);
        components.push(buttons(true, false));
        Self::container(ACCENT_IDLE, components)
    }

    fn stopped(replay: &Replay, disconnected: bool) -> Self {
        let lead = if disconnected {
            "The queue is cleared and I left the voice channel. Start it again with:"
        } else {
            "The queue is cleared and I left the voice channel. Press **Play again** to start it over, or run it yourself:"
        };
        let mut body = String::new();
        if let Some(playlist) = replay.playlist.as_deref() {
            let _ = writeln!(body, "{}", playlist_link(playlist));
        }
        let _ = write!(body, "{lead}\n{}", replay_command(replay));
        Self::container(
            ACCENT_IDLE,
            vec![
                text("### Playback stopped".into()),
                separator(true),
                text(body),
                Component::ActionRow(ActionRow {
                    id: None,
                    components: vec![Component::Button(Button {
                        id: None,
                        custom_id: Some("player:again".into()),
                        label: Some("Play again".into()),
                        style: ButtonStyle::Primary,
                        disabled: disconnected,
                        emoji: Some(EmojiReactionType::Unicode {
                            name: "\u{1F501}".into(),
                        }),
                        url: None,
                        sku_id: None,
                    })],
                }),
            ],
        )
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

/// Bold `label`, linked when `url` is safe to embed: HTTPS only, and no `>`,
/// which would end the suppressed-embed `<…>` early.
fn link(label: &str, url: Option<&url::Url>) -> String {
    match url
        .map(url::Url::as_str)
        .filter(|url| url.starts_with("https://") && !url.contains('>'))
    {
        Some(url) => format!("**[{label}](<{url}>)**"),
        None => format!("**{label}**"),
    }
}

/// A playlist's name, linked to the playlist it was imported from.
pub(super) fn playlist_link(playlist: &Playlist) -> String {
    link(&escape(&playlist.name), Some(&playlist.url))
}

/// The `/play` invocation that reproduces this player, ready to copy.
///
/// Backticks would break out of the code span; a parsed URL never contains
/// one, but the offer is only ever as good as what the user typed.
fn replay_command(replay: &Replay) -> String {
    let url: String = replay
        .url
        .chars()
        .filter(|character| !matches!(character, '`' | '\n' | '\r'))
        .take(1024)
        .collect();
    format!(
        "`/play url: {url} shuffle: {}`",
        if replay.shuffle { "On" } else { "Off" }
    )
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

fn buttons(empty: bool, paused: bool) -> Component {
    let (toggle, toggle_label, toggle_emoji) = if paused {
        ("resume", "Resume", "▶️")
    } else {
        ("pause", "Pause", "⏸️")
    };
    Component::ActionRow(ActionRow {
        id: None,
        components: [
            (toggle, toggle_label, toggle_emoji, empty),
            ("skip", "Skip", "⏭️", empty),
            ("stop", "Stop", "⏹️", false),
            ("add", "Add", "➕", false),
            ("queue", "Queue", "📜", false),
        ]
        .into_iter()
        .map(|(command, label, emoji, disabled)| {
            Component::Button(Button {
                id: None,
                custom_id: Some(format!("player:{command}")),
                label: Some(label.into()),
                style: ButtonStyle::Secondary,
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
