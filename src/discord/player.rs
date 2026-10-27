//! The player message: its buttons, which message it is in each guild, and
//! the task that keeps it current.

mod task;
mod view;

use std::sync::Arc;

use dashmap::{DashMap, mapref::entry::Entry};
use tokio_util::sync::CancellationToken;
use twilight_model::{
    channel::Message,
    id::{
        Id,
        marker::{GuildMarker, MessageMarker},
    },
};

use self::task::{PlayerTask, delete_player};
use super::commands::Action;
use crate::{discord::state::State, voice::session::Control};

/// A button the player can show.
struct PlayerButton {
    action: Action,
    custom_id: &'static str,
    label: &'static str,
    emoji: &'static str,
}

/// Every player button. Drawing the buttons and reading presses back both use
/// this table, so the two cannot drift apart.
const BUTTONS: [PlayerButton; 6] = [
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
        action: Action::Play,
        custom_id: "player:add",
        label: "Add",
        emoji: "➕",
    },
    PlayerButton {
        action: Action::Queue,
        custom_id: "player:queue",
        label: "Queue",
        emoji: "📜",
    },
];

/// What a press on the button with `custom_id` asks for.
pub fn button_command(custom_id: &str) -> Option<Action> {
    BUTTONS
        .iter()
        .find(|button| button.custom_id == custom_id)
        .map(|button| button.action)
}

/// Which message is each guild's player, and the voice session it shows.
#[derive(Default)]
pub struct Panels(DashMap<Id<GuildMarker>, Owner>);

struct Owner {
    message_id: Id<MessageMarker>,
    cancel: CancellationToken,
}

impl Panels {
    /// The first successful /play response owns the player for its session.
    fn claim(
        &self,
        guild_id: Id<GuildMarker>,
        cancel: &CancellationToken,
        message_id: Id<MessageMarker>,
    ) -> bool {
        if cancel.is_cancelled() {
            return false;
        }
        let owner = Owner {
            message_id,
            cancel: cancel.clone(),
        };
        match self.0.entry(guild_id) {
            // A player left over from an ended session gives way.
            Entry::Occupied(mut entry) if entry.get().cancel.is_cancelled() => {
                entry.insert(owner);
                true
            }
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(owner);
                true
            }
        }
    }

    /// Give up ownership, unless another message has taken it over since.
    fn release(&self, guild_id: Id<GuildMarker>, message_id: Id<MessageMarker>) {
        self.0
            .remove_if(&guild_id, |_, owner| owner.message_id == message_id);
    }

    /// The session behind `message_id`, if it is still the guild's player.
    pub fn session(
        &self,
        guild_id: Id<GuildMarker>,
        message_id: Id<MessageMarker>,
    ) -> Option<CancellationToken> {
        self.0
            .get(&guild_id)
            .filter(|owner| owner.message_id == message_id)
            .map(|owner| owner.cancel.clone())
    }
}

/// Turn a /play reply into the guild's player, unless the session already
/// has one.
pub async fn start(
    state: Arc<State>,
    guild_id: Id<GuildMarker>,
    cancel: &CancellationToken,
    message: Message,
    notice: String,
) {
    let session = state.sessions.session(guild_id, cancel).await;
    let Some(session) = session.filter(|_| state.panels.claim(guild_id, cancel, message.id)) else {
        // A stop can race with Discord returning the initial /play reply.
        if cancel.is_cancelled() {
            delete_player(&state, message.channel_id, message.id).await;
        }
        return;
    };
    let lease = PanelLease {
        state: Arc::clone(&state),
        guild_id,
        message_id: message.id,
    };
    let task = PlayerTask::new(Arc::clone(&state), guild_id, &message, session, notice);
    state.player_tasks.spawn(async move {
        // Also release ownership if this task panics, so another /play can retry.
        let _lease = lease;
        task.run().await;
    });
}

struct PanelLease {
    state: Arc<State>,
    guild_id: Id<GuildMarker>,
    message_id: Id<MessageMarker>,
}

impl Drop for PanelLease {
    fn drop(&mut self) {
        self.state.panels.release(self.guild_id, self.message_id);
    }
}

#[cfg(test)]
#[path = "../../tests/discord/player.rs"]
mod tests;
