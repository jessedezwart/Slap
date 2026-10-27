//! The player message: its buttons, and the task that keeps it current.

mod task;
mod view;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use twilight_model::{
    channel::Message,
    id::{Id, marker::GuildMarker},
};

use self::task::{PlayerTask, delete_player};
use super::commands::Action;
use crate::{session::Control, state::State};

/// A button the player can show.
struct PlayerButton {
    action: Action,
    custom_id: &'static str,
    label: &'static str,
    emoji: &'static str,
}

/// Every player button. Drawing the buttons and reading presses back both use
/// this table, so the two cannot drift apart.
const BUTTONS: [PlayerButton; 5] = [
    PlayerButton {
        action: Action::Control(Control::Pause),
        custom_id: "player:pause",
        label: "Pause",
        emoji: "⏸️",
    },
    PlayerButton {
        action: Action::Control(Control::Resume),
        custom_id: "player:resume",
        label: "Resume",
        emoji: "▶️",
    },
    PlayerButton {
        action: Action::Control(Control::Skip),
        custom_id: "player:skip",
        label: "Skip",
        emoji: "⏭️",
    },
    PlayerButton {
        action: Action::Control(Control::Stop),
        custom_id: "player:stop",
        label: "Stop",
        emoji: "⏹️",
    },
    // A link does not fit on a button, so this one opens the play form.
    PlayerButton {
        action: Action::OpenForm,
        custom_id: "player:add",
        label: "Add",
        emoji: "➕",
    },
];

/// What a press on the button with `custom_id` asks for.
pub fn button_action(custom_id: &str) -> Option<Action> {
    BUTTONS
        .iter()
        .find(|button| button.custom_id == custom_id)
        .map(|button| button.action)
}

/// Turn a /play reply into the guild's player, unless the session already
/// has one.
pub async fn start(
    state: Arc<State>,
    guild_id: Id<GuildMarker>,
    cancel: &CancellationToken,
    message: Message,
) {
    let Some(session) = state
        .sessions
        .claim_player(guild_id, cancel, message.id)
        .await
    else {
        // A stop can race with Discord returning the initial /play reply.
        if cancel.is_cancelled() {
            delete_player(&state, message.channel_id, message.id).await;
        }
        return;
    };
    let task = PlayerTask::new(Arc::clone(&state), guild_id, &message, session);
    state.player_tasks.spawn(task.run());
}

#[cfg(test)]
#[path = "../../tests/discord/player.rs"]
mod tests;
