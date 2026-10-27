use std::{sync::Arc, time::UNIX_EPOCH};

use super::*;
use crate::discord::player::button_action;

/// A queue entry for `metadata`, queued by user 30 outside any playlist.
fn entry(metadata: Metadata) -> QueueEntry {
    QueueEntry {
        metadata,
        origin: Arc::new(Origin {
            requester: Id::new(30),
            url: "https://youtu.be/x".parse().unwrap(),
            playlist_name: None,
            skipped: 0,
            truncated: false,
        }),
    }
}

/// All text in the view, in display order, including nested sections.
fn texts(components: &[Component]) -> String {
    components
        .iter()
        .map(|component| match component {
            Component::Container(container) => texts(&container.components),
            Component::Section(section) => texts(&section.components),
            Component::TextDisplay(display) => display.content.clone() + "\n",
            _ => String::new(),
        })
        .collect()
}

fn buttons_of(view: &View) -> Vec<&Button> {
    let Component::Container(container) = &view.components[0] else {
        panic!()
    };
    let Some(Component::ActionRow(row)) = container.components.last() else {
        panic!()
    };
    row.components
        .iter()
        .map(|component| match component {
            Component::Button(button) => button,
            _ => panic!(),
        })
        .collect()
}

#[test]
fn static_position_handles_unknown_zero_and_overrun() {
    assert_eq!(
        static_position(Duration::from_secs(10), Some(Duration::from_secs(300))),
        "`0:10 / 5:00`"
    );
    assert_eq!(
        static_position(Duration::from_secs(90), Some(Duration::from_secs(60))),
        "`1:00 / 1:00`"
    );
    for duration in [None, Some(Duration::ZERO)] {
        assert_eq!(static_position(Duration::from_secs(30), duration), "`0:30`");
    }
}

#[test]
fn playing_tracks_count_down_on_their_own() {
    let now = UNIX_EPOCH + Duration::from_secs(1_000);
    let playing = |duration| progress(Status::Playing, Duration::from_secs(30), duration, now);
    assert_eq!(
        playing(Some(Duration::from_secs(60))),
        "`1:00` · ends <t:1030:R>"
    );
    for duration in [None, Some(Duration::ZERO)] {
        assert_eq!(playing(duration), "Started <t:970:R>");
    }
    // A paused track is not going anywhere, so it shows the plain position.
    assert_eq!(
        progress(
            Status::Paused,
            Duration::from_secs(30),
            Some(Duration::from_secs(60)),
            now
        ),
        "`0:30 / 1:00`"
    );
}

#[test]
fn paused_views_are_stable_and_titles_are_bounded() {
    let channel = Id::new(20);
    let metadata = QueueEntry {
        origin: Arc::new(Origin {
            requester: Id::new(30),
            url: "https://www.youtube.com/playlist?list=PL1".parse().unwrap(),
            playlist_name: Some("Late Night".into()),
            skipped: 2,
            truncated: false,
        }),
        ..entry(Metadata {
            title: Some("*@everyone\n".repeat(1000)),
            duration: Some(Duration::from_secs(60)),
            ..Default::default()
        })
    };
    let info = TrackState {
        playing: PlayMode::Pause,
        position: Duration::from_secs(30),
        ready: ReadyState::Playable,
        ..Default::default()
    };
    let view = View::playing(&metadata, &info, &[], 500, channel, UNIX_EPOCH);
    let content = texts(&view.components);
    assert!(content.starts_with("### Paused"));
    assert!(!content.contains("@everyone"));
    assert!(content.contains("From **[Late Night](<https://www.youtube.com/playlist?list=PL1>)**"));
    assert!(content.contains("Added by <@30>"));
    assert!(content.contains("<#20>"));
    assert!(content.contains("`0:30 / 1:00`\n"));
    // No upcoming tracks were fetched, so there is nothing to list.
    assert!(content.contains("**Up next**\n"));
    // The track's own import notice, not the player's.
    assert!(content.contains("-# Skipped 2 unavailable entries.\n"));
    assert!(content.chars().count() < 1000);
    assert_eq!(
        view,
        View::playing(&metadata, &info, &[], 500, channel, UNIX_EPOCH)
    );
    let buttons = buttons_of(&view);
    assert_eq!(buttons[0].custom_id.as_deref(), Some("player:resume"));
    assert!(
        texts(
            &View::playing(
                &metadata,
                &TrackState::default(),
                &[],
                0,
                channel,
                UNIX_EPOCH
            )
            .components
        )
        .starts_with("### Loading…")
    );
}

#[test]
fn up_next_lists_five_bounded_titles() {
    let owned: Vec<_> = (1..=7)
        .map(|i| Metadata {
            title: Some(format!("Song {i}")),
            duration: Some(Duration::from_secs(60)),
            ..Metadata::default()
        })
        .collect();
    let next: Vec<&Metadata> = owned.iter().collect();
    let content = up_next(&next, 12);
    assert!(content.starts_with("**Up next** · `12` queued\n```\n1. Song 1\n"));
    assert!(content.ends_with("\n5. Song 5\n```"));
    assert!(!content.contains("Song 6"));
    assert_eq!(
        up_next(&next[..2], 2),
        "**Up next** · `2` queued\n```\n1. Song 1\n2. Song 2\n```"
    );
    assert_eq!(up_next(&[], 0), "**Up next**\n-# Nothing else is queued.");
    let metadata = Metadata {
        title: Some("```\n".repeat(100)),
        ..Metadata::default()
    };
    // Titles can neither add lines nor close the block early.
    let content = up_next(&[&metadata; 5], 5);
    assert_eq!(content.lines().count(), 8);
    assert_eq!(content.matches("```").count(), 2);
}

#[test]
fn thumbnails_and_links_only_accept_https() {
    let metadata = Metadata {
        title: Some("Song".into()),
        thumbnail: Some("https://i.ytimg.com/vi/x/hq.jpg".into()),
        webpage_url: Some("https://youtu.be/x".parse().unwrap()),
        ..Default::default()
    };
    let view = View::playing(
        &entry(metadata.clone()),
        &TrackState::default(),
        &[],
        0,
        Id::new(1),
        UNIX_EPOCH,
    );
    let Component::Container(container) = &view.components[0] else {
        panic!()
    };
    assert!(
        container
            .components
            .iter()
            .any(|component| matches!(component, Component::Section(_)))
    );
    assert!(texts(&view.components).contains("**[Song](<https://youtu.be/x>)**"));
    let metadata = Metadata {
        thumbnail: Some("javascript:alert(1)".into()),
        webpage_url: Some("http://example.com".parse().unwrap()),
        ..metadata
    };
    let view = View::playing(
        &entry(metadata),
        &TrackState::default(),
        &[],
        0,
        Id::new(1),
        UNIX_EPOCH,
    );
    let Component::Container(container) = &view.components[0] else {
        panic!()
    };
    assert!(
        !container
            .components
            .iter()
            .any(|component| matches!(component, Component::Section(_)))
    );
    assert!(texts(&view.components).contains("**Song**"));
}

#[test]
fn controls_disable_for_empty_queues() {
    let view = View::empty();
    for (button, disabled) in buttons_of(&view)
        .into_iter()
        .zip([true, true, false, false])
    {
        assert_eq!(button.disabled, disabled);
        assert_eq!(button.style, ButtonStyle::Secondary);
        assert!(button.emoji.is_some());
        assert!(button_action(button.custom_id.as_deref().unwrap()).is_some());
    }
}
