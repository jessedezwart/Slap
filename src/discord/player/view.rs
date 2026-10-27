//! What the player message looks like.

use std::{
    fmt::Write as _,
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
        text::{escape, format_duration, link, playlist_link},
    },
    media::Metadata,
    session::{Control, QueueEntry},
};

/// Upcoming tracks the player lists.
pub(super) const UP_NEXT_LEN: usize = 5;

const ACCENT_PLAYING: u32 = 0x8B5CF6;
const ACCENT_PAUSED: u32 = 0xF0B232;
const ACCENT_IDLE: u32 = 0x80848E;

/// A Components V2 message: everything lives in one accented container.
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
    /// The track has finished and the next one has not started yet.
    Changing,
}

impl Status {
    fn of(info: &TrackState) -> Self {
        if info.playing.is_done() {
            Self::Changing
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
            Self::Changing => "Changing tracks…",
        }
    }

    fn accent(self) -> u32 {
        match self {
            Self::Playing => ACCENT_PLAYING,
            Self::Paused => ACCENT_PAUSED,
            Self::Loading | Self::Changing => ACCENT_IDLE,
        }
    }
}

impl View {
    /// `next` is the front of the `upcoming` tracks, at most [`UP_NEXT_LEN`].
    pub fn playing(
        entry: &QueueEntry,
        info: &TrackState,
        next: &[&Metadata],
        upcoming: usize,
        notice: &str,
        voice_channel: Id<ChannelMarker>,
        now: SystemTime,
    ) -> Self {
        let metadata = &entry.metadata;
        let status = Status::of(info);
        let title = escape(metadata.display_title());
        let mut details = link(&title, metadata.webpage_url.as_ref());
        if let Some(playlist) = entry.playlist.as_deref() {
            let _ = write!(details, "\n- From {}", playlist_link(playlist));
        }
        let _ = write!(details, "\n- Added by <@{}>", entry.requester);
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
            text(format!("### {}", status.label())),
            separator(),
            details,
            text(progress(status, info.position, metadata.duration, now)),
            separator(),
            text(up_next(next, upcoming)),
        ];
        push_notice(&mut components, notice);
        components.push(buttons(false, status == Status::Paused));
        Self::container(status.accent(), components)
    }

    pub fn empty(notice: &str) -> Self {
        let mut components = vec![
            text("### Queue finished".into()),
            separator(),
            text("Press Add to queue more music, or Stop to disconnect.".into()),
        ];
        push_notice(&mut components, notice);
        components.push(buttons(true, false));
        Self::container(ACCENT_IDLE, components)
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

fn push_notice(components: &mut Vec<Component>, notice: &str) {
    let notice = notice.trim();
    if !notice.is_empty() {
        components.push(text(format!("-# {notice}")));
    }
}

/// The next few tracks as a numbered code block, under a count of
/// everything still queued.
fn up_next(next: &[&Metadata], upcoming: usize) -> String {
    let next = &next[..next.len().min(UP_NEXT_LEN)];
    if upcoming == 0 || next.is_empty() {
        return "**Up next**\n-# Nothing else is queued.".into();
    }
    let mut content = format!("**Up next** · `{upcoming}` queued\n```");
    for (index, metadata) in next.iter().enumerate() {
        let _ = write!(
            content,
            "\n{}. {}",
            index + 1,
            code_line(metadata.display_title())
        );
    }
    content.push_str("\n```");
    content
}

/// Keep a title on one line inside a code block: markdown does not render
/// there, so only a newline or a backtick could break out of it.
fn code_line(text: &str) -> String {
    text.chars()
        .take(80)
        .map(|c| match c {
            '\n' | '\r' => ' ',
            '`' => '\'',
            _ => c,
        })
        .collect()
}

fn progress(
    status: Status,
    position: Duration,
    duration: Option<Duration>,
    now: SystemTime,
) -> String {
    if status != Status::Playing {
        return static_position(position, duration);
    }
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    match duration.filter(|duration| !duration.is_zero()) {
        Some(duration) => {
            let end = now + duration.saturating_sub(position);
            format!(
                "`{}` · ends <t:{}:R>",
                format_duration(duration),
                end.as_secs()
            )
        }
        // Live streams and the like have no end to count down to.
        None => format!("Started <t:{}:R>", now.saturating_sub(position).as_secs()),
    }
}

/// A position that is not moving, against the length when it is known.
fn static_position(position: Duration, duration: Option<Duration>) -> String {
    let Some(duration) = duration.filter(|duration| !duration.is_zero()) else {
        return format!("`{}`", format_duration(position));
    };
    format!(
        "`{} / {}`",
        format_duration(position.min(duration)),
        format_duration(duration)
    )
}

/// The control row. Pause and Resume share a slot; an empty queue leaves
/// nothing to pause or skip.
fn buttons(empty: bool, paused: bool) -> Component {
    let toggle = if paused {
        Control::Resume
    } else {
        Control::Pause
    };
    Component::ActionRow(ActionRow {
        id: None,
        components: [
            (Action::Control(toggle), empty),
            (Action::Control(Control::Skip), empty),
            (Action::Control(Control::Stop), false),
            (Action::OpenForm, false),
        ]
        .into_iter()
        .map(|(action, disabled)| button(action, disabled))
        .collect(),
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
