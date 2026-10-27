//! What the player message looks like.

use std::{
    fmt::Write as _,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use songbird::tracks::{PlayMode, ReadyState, TrackState};
use twilight_model::{
    channel::message::{
        Component, EmojiReactionType,
        component::{
            ActionRow, Button, ButtonStyle, Container, Section, Separator, SeparatorSpacingSize,
            TextDisplay, Thumbnail, UnfurledMediaItem,
        },
    },
    id::{Id, marker::ChannelMarker},
};

use super::{BUTTONS, PlayerButton};
use crate::{
    discord::{
        commands::Action,
        text::{code_line, format_duration, link},
    },
    media::PLAYLIST_LIMIT,
    session::{Control, Origin, QueueEntry},
};

/// Upcoming tracks the player lists.
pub(super) const UP_NEXT_LEN: usize = 5;
/// Characters kept of each "Up next" line.
const UP_NEXT_LINE_LEN: usize = 80;

const ACCENT_PLAYING: u32 = 0x8B5CF6;
const ACCENT_PAUSED: u32 = 0xF0B232;
const ACCENT_IDLE: u32 = 0x80848E;

#[derive(Debug, PartialEq)]
pub(super) struct View {
    pub components: Vec<Component>,
}

/// The headline of a playing view.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Playing,
    Paused,
    Loading,
}

impl Status {
    fn of(info: &TrackState) -> Self {
        if info.playing.is_done() {
            Self::Loading
        } else if info.playing == PlayMode::Pause {
            Self::Paused
        } else if info.ready != ReadyState::Playable {
            Self::Loading
        } else {
            Self::Playing
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Playing => "Now playing",
            Self::Paused => "Paused",
            Self::Loading => "Loading…",
        }
    }

    fn accent(self) -> u32 {
        match self {
            Self::Playing => ACCENT_PLAYING,
            Self::Paused => ACCENT_PAUSED,
            Self::Loading => ACCENT_IDLE,
        }
    }
}

impl View {
    /// `next` is the front of the `upcoming` tracks, at most [`UP_NEXT_LEN`].
    pub fn playing(
        entry: &QueueEntry,
        info: &TrackState,
        next: &[Arc<QueueEntry>],
        upcoming: usize,
        voice_channel: Id<ChannelMarker>,
        now: SystemTime,
    ) -> Self {
        let metadata = &entry.metadata;
        let status = Status::of(info);
        let mut details = link(metadata.display_title(), metadata.webpage_url.as_ref());
        let origin = &entry.origin;
        if let Some(name) = origin.playlist_name.as_deref() {
            let _ = write!(details, "\n- From {}", link(name, Some(&origin.url)));
        }
        let _ = write!(details, "\n- Added by <@{}>", origin.requester);
        let _ = write!(details, "\n- <#{voice_channel}>");
        let details = match metadata.thumbnail.as_deref() {
            // Add thumbnail if valid URL
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

        // Assemble
        let mut components = vec![
            text(format!("### {}", status.label())),
            separator(),
            details,
            text(progress(status, info.position, metadata.duration, now)),
            separator(),
            text(up_next(next, upcoming)),
        ];
        // Add notice if exists
        if let Some(notice) = import_notice(origin) {
            components.push(text(format!("-# {notice}")));
        }
        components.push(buttons(false, status == Status::Paused));
        Self::container(status.accent(), components)
    }

    pub fn empty() -> Self {
        Self::container(
            ACCENT_IDLE,
            vec![
                text("### Queue finished".into()),
                separator(),
                text("Press Add to queue more music, or Stop to disconnect.".into()),
                buttons(true, false),
            ],
        )
    }

    pub fn stopped() -> Self {
        Self::container(
            ACCENT_IDLE,
            vec![
                text("### Playback stopped".into()),
                separator(),
                text("The queue is cleared and I left the voice channel.".into()),
            ],
        )
    }

    fn container(accent: u32, components: Vec<Component>) -> Self {
        Self {
            components: vec![Component::Container(Container {
                id: None,
                // Two somes are Twilight's way of setting something that can be missing or null.
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

fn separator() -> Component {
    Component::Separator(Separator {
        id: None,
        divider: Some(true),
        spacing: Some(SeparatorSpacingSize::Small),
    })
}

fn up_next(next: &[Arc<QueueEntry>], upcoming: usize) -> String {
    if upcoming == 0 || next.is_empty() {
        return "**Up next**\n-# Nothing else is queued.".into();
    }
    let mut content = format!("**Up next** · `{upcoming}` queued\n```");
    for entry in next {
        let line = format!(
            "{}: {}",
            entry.origin.requester_name,
            entry.metadata.display_title()
        );
        let _ = write!(content, "\n{}", code_line(&line, Some(UP_NEXT_LINE_LEN)));
    }
    content.push_str("\n```");
    content
}

/// What went wrong importing the tracks, if anything.
fn import_notice(origin: &Origin) -> Option<String> {
    let mut notice = Vec::new();
    if origin.skipped > 0 {
        notice.push(format!("Skipped {} unavailable entries.", origin.skipped));
    }
    if origin.truncated {
        notice.push(format!(
            "Import limited to the first {PLAYLIST_LIMIT} playlist entries."
        ));
    }
    (!notice.is_empty()).then(|| notice.join(" "))
}

fn progress(
    status: Status,
    position: Duration,
    duration: Option<Duration>,
    now: SystemTime,
) -> String {
    if status != Status::Playing {
        return static_duration(position, duration);
    }
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    match duration.filter(|duration| !duration.is_zero()) {
        Some(duration) => {
            // Calculate end time based on current position and total duration
            let end = now + duration.saturating_sub(position);
            format!(
                "`{}` · ends <t:{}:R>",
                format_duration(duration),
                end.as_secs()
            )
        }
        // Live streams have no end to count down to.
        None => format!("Started <t:{}:R>", now.saturating_sub(position).as_secs()),
    }
}

/// A position that is not moving, against the length when it is known.
fn static_duration(position: Duration, duration: Option<Duration>) -> String {
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
    let playback_toggle = if paused {
        Control::Resume
    } else {
        Control::Pause
    };
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            button(Action::Control(playback_toggle), empty),
            button(Action::Control(Control::Skip), empty),
            button(Action::Control(Control::Stop), false),
            button(Action::OpenForm, false),
        ],
    })
}

fn button(action: Action, disabled: bool) -> Component {
    let PlayerButton {
        custom_id,
        label,
        emoji,
        ..
    } = BUTTONS
        .iter()
        .find(|button| button.action == action)
        .expect("every drawn action has a button");
    Component::Button(Button {
        id: None,
        custom_id: Some((*custom_id).into()),
        label: Some((*label).into()),
        style: ButtonStyle::Secondary,
        disabled,
        emoji: Some(EmojiReactionType::Unicode {
            name: (*emoji).into(),
        }),
        url: None,
        sku_id: None,
    })
}

#[cfg(test)]
#[path = "../../../tests/discord/player/view.rs"]
mod tests;
